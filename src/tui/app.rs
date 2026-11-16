//! TUI application state, input handling, and rendering.
//!
//! The TUI is intentionally small: it's an interactive view over the
//! `MappingsStore` plus a tail of the most recent request log lines.
//! All heavy lifting lives in the store and the panels module.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Wrap,
};
use unicode_width::UnicodeWidthStr;

use super::output::{LogKind, LogLine};
use super::panels::{Panel, truncate_to_width};
use super::runtime::{MappingsStore, RuntimeMappings};
use super::stats::{SessionStatsSnapshot, SessionStatsStore, TokenTotals};
use crate::capabilities::CapabilityStore;
use crate::config::JsonConfig;

/// Maximum number of log lines retained for the on-screen tail.
///
/// Raised from 32 to 1000 so the tail is useful in practice. With
/// the scrollable `List` widget the entire buffer is reachable via
/// `PageUp`/`PageDown`/`Home`/`End`.
const LOG_TAIL: usize = 1000;

// Keep token colours in one place so SESSION TOKENS and RECENT REQUESTS
// cannot drift apart as either display is changed.
const TOKEN_INPUT_COLOR: Color = Color::Green;
const TOKEN_OUTPUT_COLOR: Color = Color::Red;
const TOKEN_CACHE_COLOR: Color = Color::Cyan;
const TOKEN_REASONING_COLOR: Color = Color::Yellow;

/// Stable, ordered list of inbound models for display. We sort the live
/// map's keys for stability; the `default_model` is rendered as its
/// own row even if it isn't an inbound name.
#[derive(Debug, Clone)]
struct DisplayRow {
    inbound: String,
    outbound: String,
    dirty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Editing(EditField),
    /// "Are you sure you want to save?" prompt.
    ConfirmSave,
    /// "Save complete" toast — auto-dismissing.
    SavedToast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditField {
    Inbound,
    Outbound,
}

/// Top-level TUI state. Cloning is fine — it just bumps a couple of
/// `Arc`s and copies a small `VecDeque`.
pub struct TuiApp {
    /// Shared store. Read on every render, written by edit actions.
    store: std::sync::Arc<MappingsStore>,
    /// Session-only token totals shared with request handlers.
    stats: std::sync::Arc<SessionStatsStore>,
    /// Process-lifetime record of request parameters learned to be
    /// unsupported per upstream model. Shared with the request handlers
    /// so the dashboard shows what the running proxy has discovered.
    capabilities: std::sync::Arc<CapabilityStore>,
    /// Path to `proxy.json` for save operations. `None` disables saving
    /// (the TUI still works for in-memory changes).
    config_path: Option<PathBuf>,
    /// Configured listen address (e.g. `0.0.0.0:8085`) shown in the
    /// status bar. Cached here so we don't have to plumb a `Config`
    /// reference through the render path.
    listen_addr: String,
    /// Configured upstream base URL (e.g. `http://192.168.1.15:11434`)
    /// shown in the status bar.
    upstream_base_url: String,
    /// Recent log lines, newest at the back.
    log: VecDeque<LogLine>,
    /// Scroll position inside the log `List`. `offset` is the index
    /// of the first visible item (0 = newest, `log.len() - 1` = oldest).
    /// We display the log newest-first, so the "tail" position is
    /// `offset == 0`.
    log_list_state: ListState,
    /// Last rendered log area height. Used by `PageUp`/`PageDown` to
    /// jump by a full screen. Refreshed every render frame.
    log_viewport_h: u16,
    /// When `true`, newly-pushed log lines auto-scroll the view to the
    /// newest entry. Set to `false` when the user scrolls up, restored
    /// to `true` when they hit `End`/`G`.
    log_follow_tail: bool,
    /// Last time the dashboard was rendered. Used for the uptime clock.
    started_at: Instant,
    /// Current input mode.
    mode: Mode,
    /// Inbound model name being edited.
    edit_inbound: String,
    /// Upstream model name being edited.
    edit_outbound: String,
    /// Cursor position in the active field (byte index, not char).
    edit_cursor: usize,
    /// Toast text to show after a successful save.
    toast: Option<String>,
    /// Index of the currently-selected mapping row (0-based, sorted).
    selected: usize,
    /// True when the on-screen state has changed since the last
    /// `terminal.draw`. The runner loop only issues a draw when this
    /// flips to true. Set in `push_log`, scroll handlers, mode
    /// transitions, mutations, save, and toast clearing; cleared by
    /// the runner after each successful draw. See Fix 1 in
    /// `.claude/plans/foamy-wondering-pike.md`.
    dirty: bool,
}

impl TuiApp {
    /// Construct a new app.
    pub fn new(
        store: std::sync::Arc<MappingsStore>,
        config_path: Option<PathBuf>,
        listen_addr: String,
        upstream_base_url: String,
    ) -> Self {
        Self::new_with_stats(
            store,
            std::sync::Arc::new(SessionStatsStore::new()),
            config_path,
            listen_addr,
            upstream_base_url,
        )
    }

    /// Construct an app using the request handlers' shared session stats.
    pub fn new_with_stats(
        store: std::sync::Arc<MappingsStore>,
        stats: std::sync::Arc<SessionStatsStore>,
        config_path: Option<PathBuf>,
        listen_addr: String,
        upstream_base_url: String,
    ) -> Self {
        Self::new_with_stores(
            store,
            stats,
            std::sync::Arc::new(CapabilityStore::new()),
            config_path,
            listen_addr,
            upstream_base_url,
        )
    }

    /// Construct an app sharing every long-lived store with the request
    /// handlers. This is the constructor `runner::run` uses; the older
    /// ones stay in place so tests (and any caller that does not care
    /// about the capability view) keep compiling unchanged.
    pub fn new_with_stores(
        store: std::sync::Arc<MappingsStore>,
        stats: std::sync::Arc<SessionStatsStore>,
        capabilities: std::sync::Arc<CapabilityStore>,
        config_path: Option<PathBuf>,
        listen_addr: String,
        upstream_base_url: String,
    ) -> Self {
        let mut log_list_state = ListState::default();
        // Show the newest entry at the top by default (offset 0).
        log_list_state.select(Some(0));
        Self {
            store,
            stats,
            capabilities,
            config_path,
            listen_addr,
            upstream_base_url,
            log: VecDeque::with_capacity(LOG_TAIL),
            log_list_state,
            log_viewport_h: 0,
            log_follow_tail: true,
            started_at: Instant::now(),
            mode: Mode::Normal,
            edit_inbound: String::new(),
            edit_outbound: String::new(),
            edit_cursor: 0,
            toast: None,
            selected: 0,
            // First frame must draw — initial state (title, "0
            // mappings", empty log) needs to be rendered before the
            // dirty flag can do anything useful.
            dirty: true,
        }
    }

    /// Push a log line from the receiver. Called by the TUI loop.
    pub fn push_log(&mut self, line: LogLine) {
        if self.log.len() == LOG_TAIL {
            self.log.pop_front();
        }
        self.log.push_back(line);
        // If the user is tail-following, keep them on the newest entry.
        // We display newest-first, so "newest" is index 0.
        if self.log_follow_tail {
            *self.log_list_state.offset_mut() = 0;
            self.log_list_state.select(Some(0));
        }
        // If the user has scrolled away, leave their position alone.
        // When new lines arrive the offset stays put so the relative
        // view of the older history is preserved.
        // A new line is always visible to the operator, so the next
        // frame must redraw. Mark dirty.
        self.dirty = true;
    }

    /// Mark the TUI as needing a redraw. Called by every code path
    /// that mutates visible state: `apply_mutation`, `save_to_disk`,
    /// scroll handlers, mode transitions, the toast-dismiss branch
    /// in `on_key`, and the runner's tick arm (which exists only
    /// to keep the uptime counter visibly fresh).
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Returns `true` exactly once for each "dirty" edge. The runner
    /// calls this before issuing `terminal.draw(...)`; if it returns
    /// `false` the previous frame is still on screen and no draw is
    /// needed. After returning `true` the flag is cleared so a
    /// subsequent call (before any state change) returns `false`.
    pub fn take_dirty(&mut self) -> bool {
        let was = self.dirty;
        self.dirty = false;
        was
    }

    /// Display rows in the same order each frame (sorted by inbound).
    fn rows(&self) -> Vec<DisplayRow> {
        let snap = self.store.snapshot();
        let live = &snap.live;
        let mut keys: Vec<String> = live.map.keys().cloned().collect();
        keys.sort();
        keys.into_iter()
            .map(|inbound| DisplayRow {
                outbound: live.map.get(&inbound).cloned().unwrap_or_default(),
                dirty: snap.inbound_is_dirty(&inbound),
                inbound,
            })
            .collect()
    }

    /// Handle a key event. Returns `true` if the TUI should quit.
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        // Crossterm on Windows fires both Press and Release for some
        // terminals; only handle the Press event to avoid double work.
        if key.kind != KeyEventKind::Press {
            return false;
        }

        // Global quit: Ctrl+C always quits, regardless of mode.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return true;
        }

        match self.mode {
            Mode::Normal => self.on_key_normal(key),
            Mode::Editing(field) => self.on_key_editing(key, field),
            Mode::ConfirmSave => self.on_key_confirm(key),
            Mode::SavedToast => {
                // Any key dismisses the toast and returns to normal.
                self.toast = None;
                self.mode = Mode::Normal;
                self.dirty = true;
                false
            }
        }
    }

    fn on_key_normal(&mut self, key: KeyEvent) -> bool {
        // Almost every key in this arm changes visible state (cursor
        // moves, mode changes, dialog opens). Mark dirty up front;
        // the few cases that early-return without doing anything
        // (q/Esc) still pay the flip but the next render is a no-op.
        self.dirty = true;
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => true,
            // Mapping-list navigation (existing behavior).
            KeyCode::Char('j') | KeyCode::Down => {
                let n = self.rows().len();
                if n > 0 && self.selected + 1 < n {
                    self.selected += 1;
                }
                false
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                false
            }
            // Log scrolling. These keys are not used by the mapping
            // list, so there's no collision with the `j`/`k`/`Up`/`Down`
            // bindings above.
            KeyCode::PageDown => {
                self.log_scroll_down(self.log_viewport_h.max(1));
                false
            }
            KeyCode::PageUp => {
                self.log_scroll_up(self.log_viewport_h.max(1));
                false
            }
            KeyCode::Home => {
                self.log_jump_to_oldest();
                false
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.log_jump_to_newest();
                false
            }
            KeyCode::Char('a') => {
                self.mode = Mode::Editing(EditField::Inbound);
                self.edit_inbound.clear();
                self.edit_outbound.clear();
                self.edit_cursor = 0;
                false
            }
            KeyCode::Char('d') => {
                self.delete_selected();
                false
            }
            KeyCode::Char('e') | KeyCode::Enter => {
                self.start_editing_selected();
                false
            }
            KeyCode::Char('f') => {
                self.edit_default_model();
                false
            }
            KeyCode::Char('s') => {
                if self.store.snapshot().is_dirty() {
                    self.mode = Mode::ConfirmSave;
                }
                false
            }
            // Clear the learned parameter blacklist. The operator's
            // escape hatch around the one-way latch: after an upstream
            // change, clearing re-probes each model on its next request
            // instead of waiting for a proxy restart.
            KeyCode::Char('c') => {
                let removed = self.capabilities.clear();
                self.toast = Some(if removed == 0 {
                    "nothing learned to clear".to_string()
                } else {
                    format!("cleared {removed} model(s); params will be re-probed")
                });
                self.mode = Mode::SavedToast;
                false
            }
            _ => false,
        }
    }

    /// Move the log scroll offset down by `n` cells (toward newer
    /// entries; i.e. decreases the offset since newest is at index 0).
    /// Clamps to the valid range.
    fn log_scroll_down(&mut self, n: u16) {
        if self.log.is_empty() {
            return;
        }
        let max_offset = self.log.len().saturating_sub(1);
        let cur = self.log_list_state.offset();
        let new = cur.saturating_sub(n as usize);
        let clamped = new.min(max_offset);
        *self.log_list_state.offset_mut() = clamped;
        self.log_list_state.select(Some(clamped));
        // Reaching the newest entry re-enables tail-following.
        self.log_follow_tail = clamped == 0;
        self.dirty = true;
    }

    /// Move the log scroll offset up by `n` cells (toward older
    /// entries; i.e. increases the offset since newest is at index 0).
    /// Clamps to the valid range.
    fn log_scroll_up(&mut self, n: u16) {
        if self.log.is_empty() {
            return;
        }
        let max_offset = self.log.len().saturating_sub(1);
        let cur = self.log_list_state.offset();
        let new = cur.saturating_add(n as usize);
        let clamped = new.min(max_offset);
        *self.log_list_state.offset_mut() = clamped;
        self.log_list_state.select(Some(clamped));
        // Moving away from the newest entry disables tail-following.
        self.log_follow_tail = clamped == 0;
        self.dirty = true;
    }

    /// Jump to the oldest log entry (largest valid offset).
    fn log_jump_to_oldest(&mut self) {
        if self.log.is_empty() {
            return;
        }
        let max_offset = self.log.len().saturating_sub(1);
        *self.log_list_state.offset_mut() = max_offset;
        self.log_list_state.select(Some(max_offset));
        self.log_follow_tail = false;
        self.dirty = true;
    }

    /// Jump to the newest log entry (offset 0). Re-enables tail-follow.
    fn log_jump_to_newest(&mut self) {
        if self.log.is_empty() {
            return;
        }
        *self.log_list_state.offset_mut() = 0;
        self.log_list_state.select(Some(0));
        self.log_follow_tail = true;
        self.dirty = true;
    }

    fn on_key_editing(&mut self, key: KeyEvent, field: EditField) -> bool {
        // Within the editor:
        //   Tab toggles the focused field.
        //   Enter applies the change (in-memory).
        //   Esc cancels.
        // Every input in this arm changes the dialog content; mark
        // dirty unconditionally.
        self.dirty = true;
        let active = match field {
            EditField::Inbound => &mut self.edit_inbound,
            EditField::Outbound => &mut self.edit_outbound,
        };

        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.edit_inbound.clear();
                self.edit_outbound.clear();
                self.edit_cursor = 0;
            }
            KeyCode::Tab => {
                let next = match field {
                    EditField::Inbound => EditField::Outbound,
                    EditField::Outbound => EditField::Inbound,
                };
                self.mode = Mode::Editing(next);
                self.edit_cursor = match next {
                    EditField::Inbound => self.edit_inbound.len(),
                    EditField::Outbound => self.edit_outbound.len(),
                };
            }
            KeyCode::Backspace if self.edit_cursor > 0 => {
                let prev = prev_char_boundary(active, self.edit_cursor);
                active.drain(prev..self.edit_cursor);
                self.edit_cursor = prev;
            }
            KeyCode::Delete if self.edit_cursor < active.len() => {
                let next = next_char_boundary(active, self.edit_cursor);
                active.drain(self.edit_cursor..next);
            }
            KeyCode::Left => self.edit_cursor = prev_char_boundary(active, self.edit_cursor),
            KeyCode::Right => self.edit_cursor = next_char_boundary(active, self.edit_cursor),
            KeyCode::Home => self.edit_cursor = 0,
            KeyCode::End => self.edit_cursor = active.len(),
            KeyCode::Enter => self.apply_edit(),
            KeyCode::Char(c) => {
                active.insert(self.edit_cursor, c);
                self.edit_cursor += c.len_utf8();
            }
            _ => {}
        }
        false
    }

    fn on_key_confirm(&mut self, key: KeyEvent) -> bool {
        self.dirty = true;
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                self.save_to_disk();
                self.mode = Mode::Normal;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.mode = Mode::Normal;
            }
            _ => {}
        }
        false
    }

    fn start_editing_selected(&mut self) {
        let rows = self.rows();
        let Some(row) = rows.get(self.selected) else {
            return;
        };
        self.edit_inbound = row.inbound.clone();
        self.edit_outbound = row.outbound.clone();
        self.edit_cursor = self.edit_outbound.len();
        self.mode = Mode::Editing(EditField::Outbound);
        self.dirty = true;
    }

    fn delete_selected(&mut self) {
        let rows = self.rows();
        let Some(row) = rows.get(self.selected) else {
            return;
        };
        let inbound = row.inbound.clone();
        self.apply_mutation(|live| {
            live.map.remove(&inbound);
        });
    }

    fn edit_default_model(&mut self) {
        let snap = self.store.snapshot();
        self.edit_inbound = "<default fallback>".into();
        self.edit_outbound = snap.live.default_model.clone().unwrap_or_default();
        self.edit_cursor = self.edit_outbound.len();
        self.mode = Mode::Editing(EditField::Outbound);
        self.dirty = true;
        // Pressing Enter on this pseudo-row should set default_model,
        // not insert a new mapping. We flag that with a sentinel
        // value in edit_inbound.
    }

    fn apply_edit(&mut self) {
        let inbound = self.edit_inbound.trim().to_string();
        let outbound = self.edit_outbound.trim().to_string();
        if inbound == "<default fallback>" {
            self.apply_mutation(|live| {
                live.default_model = if outbound.is_empty() {
                    None
                } else {
                    Some(outbound.clone())
                };
            });
        } else if !inbound.is_empty() && !outbound.is_empty() {
            self.apply_mutation(|live| {
                live.map.insert(inbound.clone(), outbound.clone());
            });
        }
        self.mode = Mode::Normal;
        self.edit_inbound.clear();
        self.edit_outbound.clear();
        self.edit_cursor = 0;
        self.dirty = true;
    }

    fn apply_mutation(&mut self, f: impl FnOnce(&mut super::runtime::RuntimeMappings)) {
        // Read the current live snapshot, mutate a clone, and swap.
        let mut next: super::runtime::RuntimeMappings = (*self.store.load_live()).as_ref().clone();
        f(&mut next);
        self.store.set_live(next);
        // The mappings table (and the `*` markers / dirty counter in
        // the status row) visibly change — schedule a redraw.
        self.dirty = true;
    }

    fn save_to_disk(&mut self) {
        let Some(path) = self.config_path.as_ref() else {
            self.toast = Some("save disabled: --no-config or no proxy.json".into());
            self.mode = Mode::SavedToast;
            self.dirty = true;
            return;
        };

        // Read the existing full config to preserve all non-mappings fields
        let mut json_config: JsonConfig = if path.exists() {
            match std::fs::read_to_string(path) {
                Ok(content) => match JsonConfig::parse(&content) {
                    Ok(cfg) => cfg,
                    Err(e) => {
                        self.toast = Some(format!("save failed: parse error: {e}"));
                        self.mode = Mode::SavedToast;
                        self.dirty = true;
                        return;
                    }
                },
                Err(e) => {
                    self.toast = Some(format!("save failed: read error: {e}"));
                    self.mode = Mode::SavedToast;
                    self.dirty = true;
                    return;
                }
            }
        } else {
            JsonConfig::default()
        };

        // Update only the model_aliases from live mappings
        let live: RuntimeMappings = (*self.store.load_live()).as_ref().clone();
        json_config.model_aliases = Some(crate::config::JsonModelAliases {
            map: live.map,
            default_model: live.default_model,
        });

        let json = match serde_json::to_string_pretty(&json_config) {
            Ok(j) => j,
            Err(e) => {
                self.toast = Some(format!("save failed: serialize error: {e}"));
                self.mode = Mode::SavedToast;
                self.dirty = true;
                return;
            }
        };

        // Write to a temp file, then atomic-rename. This avoids leaving
        // a half-written proxy.json if the process is killed mid-write.
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, &json) {
            self.toast = Some(format!("save failed: write error: {e}"));
            self.mode = Mode::SavedToast;
            self.dirty = true;
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            // Best-effort: try to clean up the temp file.
            let _ = std::fs::remove_file(&tmp);
            self.toast = Some(format!("save failed: rename error: {e}"));
            self.mode = Mode::SavedToast;
            self.dirty = true;
            return;
        }
        self.store.mark_saved();
        self.toast = Some(format!("saved {path:?}"));
        self.mode = Mode::SavedToast;
        self.dirty = true;
    }

    /// Render the TUI. `area` is the full terminal area.
    ///
    /// The layout is **content-size-safe**: the dashboard panel's
    /// height is capped at the available vertical space, and the log
    /// panel uses a `List` widget that scrolls internally. This makes
    /// a `buffer::index out of bounds` panic structurally impossible
    /// regardless of how many log lines or mappings exist.
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        // Defence in depth: clamp the render area to the frame's
        // actual buffer area. If a future caller passes a stale
        // `Rect` (e.g. from a Resize event that races with a draw),
        // this prevents the inner widgets from overflowing the
        // underlying buffer.
        let area = area.intersection(frame.area());

        // Width budget. We use 4 cells of margin so panels don't
        // press against the terminal edge, and cap at the terminal
        // width minus the margin.
        let margin: u16 = 2;
        let max_w = area.width.saturating_sub(margin * 2);
        let inner_w = max_w.saturating_sub(2).max(20);

        // ---- Dashboard panel (log-free) ----
        let snap = self.store.snapshot();
        // The title is widened to fit even the narrowest panel we
        // expect to see (inner_w == 20 in pathological cases). The
        // long-form status strings go into body rows instead so the
        // panel border never overflows its measured width.
        let mut dash = Panel::new(
            format!(
                "proxy  v0.1.0  uptime {}s",
                self.started_at.elapsed().as_secs()
            ),
            inner_w,
        );
        // ---- Status strip ----
        // One compact line. The dashboard's job is the table below, so
        // the old two-row STATUS/MAPPINGS block folds into a single
        // strip and the key hints move to the footer.
        let config_label = self
            .config_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<none>".into());
        let fallback = snap.live.default_model.as_deref().unwrap_or("<none>");
        dash.row(&format!(
            "\u{25cf} ONLINE   {listen}   \u{2192} {upstream}   cfg {config} \u{00b7} {save}   {n} mappings   fallback {fallback}",
            listen = self.listen_addr,
            upstream = self.upstream_base_url,
            config = config_label,
            save = if snap.is_dirty() { "unsaved" } else { "saved" },
            n = snap.live.map.len(),
        ));
        dash.rule();

        // ---- Merged routing + token table ----
        // A mapping row and its token totals share the same key (the
        // inbound model name), so they render as one table instead of
        // two sections that print the model name twice. The LEARNED
        // column comes from the capability store and is keyed by the
        // resolved upstream model, so sibling aliases legitimately
        // share the same value.
        let rows = self.rows();
        let registry = self.capabilities.load();
        let totals = self.stats.snapshot_sections();
        let inner = inner_w as usize;

        dash.row(
            "MODEL MAPPINGS + SESSION TOKENS   j/k select \u{00b7} Enter edit \u{00b7} a/d add/remove",
        );
        dash.row(&format!(
            "  {:<5}{:<26} {:<20} {:<12} {:>5} {:>7} {:>7} {:>7} {:>12}",
            "#", "INBOUND MODEL", "UPSTREAM MODEL", "LEARNED", "REQ", "IN", "OUT", "THINK", "CACHE"
        ));
        for (i, row) in rows.iter().enumerate() {
            let learned = format_learned_params(registry.unsupported_for(&row.outbound));
            dash.row(&format_table_row(
                i == self.selected,
                row.dirty,
                i + 1,
                &row.inbound,
                &row.outbound,
                &learned,
                totals.inbound.get(&row.inbound),
            ));
        }

        // ---- Trailing totals ----
        // The grand total, then the models that have no mapping row of
        // their own. Grouped rows are capped so a busy proxy cannot
        // crowd the request log out of the frame.
        const GROUP_CAP: usize = 6;
        dash.row(&labelled_rule("ALL COMBINED", inner));
        dash.row(&format_summary_row("ALL MODELS", &combined_totals(&totals)));

        let mut unmapped: Vec<(String, TokenTotals)> = totals
            .inbound
            .iter()
            .filter(|(model, _)| !snap.live.map.contains_key(*model))
            .map(|(model, total)| (model.clone(), *total))
            .collect();
        unmapped.sort_by(|a, b| a.0.cmp(&b.0));
        if !unmapped.is_empty() {
            let count = unmapped.len();
            dash.row(&labelled_rule("unmapped inbound", inner));
            for (model, totals) in unmapped.into_iter().take(GROUP_CAP) {
                dash.row(&format_summary_row(&model, &totals));
            }
            if count > GROUP_CAP {
                dash.row(&format!(
                    "  \u{2026} {} more unmapped model(s)",
                    count - GROUP_CAP
                ));
            }
        }

        let mut actual: Vec<(String, TokenTotals)> = totals
            .actual
            .iter()
            .map(|(model, total)| (model.clone(), *total))
            .collect();
        actual.sort_by(|a, b| a.0.cmp(&b.0));
        if !actual.is_empty() {
            let count = actual.len();
            dash.row(&labelled_rule("actual fallback models", inner));
            for (model, totals) in actual.into_iter().take(GROUP_CAP) {
                dash.row(&format_summary_row(&model, &totals));
            }
            if count > GROUP_CAP {
                dash.row(&format!(
                    "  \u{2026} {} more fallback model(s)",
                    count - GROUP_CAP
                ));
            }
        }

        // A learned fact can outlive the mapping that produced it (the
        // operator deletes the alias after a 400). Surface those too so
        // nothing silently vanishes from the display; `c` clears them.
        let orphans: Vec<(&str, String)> = registry
            .entries()
            .filter(|(model, _)| !rows.iter().any(|r| r.outbound.as_str() == *model))
            .map(|(model, params)| (model, format_learned_params(params)))
            .collect();
        if !orphans.is_empty() {
            dash.row(&labelled_rule("learned (no mapping row)", inner));
            for (model, params) in orphans {
                dash.row(&format!("  {:<60}{params}", truncate(model, 60)));
            }
        }

        // ---- Layout ----
        // The dashboard height is its own line count, **capped** at
        // the available terminal height minus what the log and footer
        // need. The log is then given at least 3 rows, and the footer
        // gets 1 row. This guarantees every widget fits inside the
        // frame buffer no matter how many mappings or log lines
        // there are.
        let dash_h_raw = dash.len() as u16;
        // Reserve at least 3 rows for the log + 1 row for the
        // footer hint strip.
        let reserved: u16 = 3 + 1;
        let max_dash = area.height.saturating_sub(reserved);
        let dash_h = dash_h_raw.min(max_dash);
        // Footer sits in the line immediately below the dashboard.
        // The log occupies the rest.
        let footer_h: u16 = 1;
        let vchunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(dash_h),
                Constraint::Min(3),           // log: at least 3 rows
                Constraint::Length(footer_h), // footer
            ])
            .split(area);
        let dash_area = vchunks[0];
        let log_area = vchunks[1];
        let footer_area = vchunks[2];

        // Track the log viewport height so `PageUp`/`PageDown` can
        // jump by a screen.
        self.log_viewport_h = log_area.height;

        // ---- Dashboard ----
        // `centered` was a free function in the previous version; it
        // was passed the panel's `width` but accidentally reused
        // `parent.width` as the height argument. With a 120-col x
        // 30-row terminal the dashboard was given a 120-row-tall
        // area, which caused the log to overflow the buffer. Use a
        // free function (see below) that always uses `parent.height`.
        let dash_lines = dash
            .lines
            .iter()
            .map(|line| style_token_usage_line(line))
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(dash_lines).style(Style::default().fg(Color::White)),
            centered(dash.width, dash_area),
        );

        // ---- Log (scrollable List + Scrollbar) ----
        // Build items newest-first. The `List` widget's `offset`
        // refers to the first visible item; with newest at index 0
        // and offset 0 the newest line is at the top.
        let recent_log: Vec<&LogLine> = self.log.iter().rev().collect();
        let recent_widths = recent_request_column_widths(recent_log.iter().copied());
        let items: Vec<ListItem> = recent_log
            .into_iter()
            .map(|line| {
                let text = normalize_recent_request_text(
                    &line.text,
                    &recent_widths[log_kind_index(line.kind)],
                );
                let display = format!("{}  {}", kind_glyph(line.kind), text);
                ListItem::new(style_token_usage_line(&display))
            })
            .collect();
        let title = if self.log.is_empty() {
            " RECENT REQUESTS  (no requests yet)   PgUp/PgDn \u{00b7} Home/End "
        } else {
            " RECENT REQUESTS  (newest first)   PgUp/PgDn \u{00b7} Home/End "
        };
        let list = List::new(items)
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .title(title),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .scroll_padding(1);
        // `List` mutates the state we hand it (it clamps the offset
        // to the visible range). We hand it a clone so the on-disk
        // `self.log_list_state` reflects the post-render reality.
        let mut state = self.log_list_state.clone();
        frame.render_stateful_widget(list, log_area, &mut state);
        self.log_list_state = state;

        // Scrollbar. We only render it when there's enough content
        // to warrant one (>= viewport height). The state is freshly
        // built each frame so the position reflects the post-render
        // `ListState`.
        if self.log.len() > log_area.height as usize {
            let mut sb_state =
                ScrollbarState::new(self.log.len()).position(self.log_list_state.offset());
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(Some("^"))
                    .end_symbol(Some("v")),
                log_area,
                &mut sb_state,
            );
        }

        // ---- Footer hint ----
        // A single-line status strip under the log. We re-use the
        // existing dash.panel footer text but render it in a small
        // paragraph so it doesn't depend on Panel line art.
        // The long form lives in the dashboard; the footer just
        // summarises the current key bindings. We truncate the
        // string with `truncate` so it never overflows narrow
        // terminals.
        let tail_note = if self.log_follow_tail {
            "tail-follow"
        } else {
            "scroll (End/G = tail)"
        };
        let footer_text = truncate(
            &format!(
                "[a] add  [e] edit  [d] delete  [f] fallback  [s] save  [c] clear learned  [q] quit   |   log: {tail_note}"
            ),
            area.width.saturating_sub(2) as usize,
        );
        frame.render_widget(
            Paragraph::new(footer_text.as_str()).style(Style::default().fg(Color::DarkGray)),
            Rect::new(area.x, footer_area.y, area.width, footer_area.height),
        );

        // ---- Edit overlay ----
        if let Mode::Editing(field) = self.mode {
            self.render_edit_overlay(frame, area, field);
        }

        // ---- Save confirmation overlay ----
        if self.mode == Mode::ConfirmSave {
            self.render_confirm_overlay(frame, area);
        }

        // ---- Toast ----
        if let Some(text) = self.toast.as_ref() {
            self.render_toast(frame, area, text);
        }
    }

    fn render_edit_overlay(&self, frame: &mut Frame, area: Rect, field: EditField) {
        let w = 60u16.min(area.width.saturating_sub(4));
        let h = 9u16;
        let x = area.x + (area.width.saturating_sub(w)) / 2;
        let y = area.y + (area.height.saturating_sub(h)) / 2;
        let popup = Rect::new(x, y, w, h);

        frame.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(
                " EDIT MAPPING ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);

        let inbound_style = if field == EditField::Inbound {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let outbound_style = if field == EditField::Outbound {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let lines = vec![
            Line::from(Span::raw("")),
            Line::from(vec![
                Span::raw("  INBOUND  "),
                Span::styled(&self.edit_inbound, inbound_style),
            ]),
            Line::from(Span::raw("")),
            Line::from(vec![
                Span::raw("  UPSTREAM "),
                Span::styled(&self.edit_outbound, outbound_style),
            ]),
            Line::from(Span::raw("")),
            Line::from(Span::styled(
                "  Tab: switch field   Enter: apply   Esc: cancel",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        let para = Paragraph::new(lines).wrap(Wrap { trim: false });
        frame.render_widget(para, inner);
    }

    fn render_confirm_overlay(&self, frame: &mut Frame, area: Rect) {
        let w = 50u16.min(area.width.saturating_sub(4));
        let h = 7u16;
        let x = area.x + (area.width.saturating_sub(w)) / 2;
        let y = area.y + (area.height.saturating_sub(h)) / 2;
        let popup = Rect::new(x, y, w, h);

        frame.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(Span::styled(
                " SAVE MAPPING ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);

        let lines = vec![
            Line::from(Span::raw("")),
            Line::from(Span::raw("  Save the in-memory changes to proxy.json?")),
            Line::from(Span::raw("")),
            Line::from(Span::styled(
                "  y: save   n: cancel",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_toast(&self, frame: &mut Frame, area: Rect, text: &str) {
        let w = (text.width() as u16 + 4).min(area.width.saturating_sub(4));
        let h = 3u16;
        let x = area.x + (area.width.saturating_sub(w)) / 2;
        let y = area.y + area.height.saturating_sub(h + 1);
        let popup = Rect::new(x, y, w, h);

        frame.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Green))
            .title(Span::styled(
                " OK ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                text.to_string(),
                Style::default().fg(Color::White),
            ))),
            inner,
        );
    }
}

/// Build a `Rect` for a panel of width `w` centered horizontally
/// inside `parent`. The resulting `Rect` keeps `parent`'s full
/// height and clamps `w` to `parent.width` (so a too-wide panel
/// never overflows the parent buffer). The previous version
/// accidentally used `parent.width` for the height, which on a
/// 120x30 terminal produced a 120-row dashboard that immediately
/// overflowed the 30-row buffer.
fn centered(w: u16, parent: Rect) -> Rect {
    let w = w.min(parent.width);
    let x = parent.x + (parent.width.saturating_sub(w)) / 2;
    Rect::new(x, parent.y, w, parent.height)
}

/// Single-character category glyph for a log line. Used as a
/// leading column in the scrollable log so the operator can spot
/// errors and warnings at a glance.
fn kind_glyph(kind: LogKind) -> &'static str {
    match kind {
        LogKind::Inbound => ">",
        LogKind::Response => "<",
        LogKind::Warning => "!",
        LogKind::Error => "X",
        LogKind::Info => ".",
    }
}

fn recent_request_column_widths<'a>(
    lines: impl IntoIterator<Item = &'a LogLine>,
) -> [Vec<usize>; 5] {
    let mut widths = std::array::from_fn(|_| Vec::new());
    for line in lines {
        let slot = &mut widths[log_kind_index(line.kind)];
        let text = format_recent_request_token_counts(&line.text);
        for (idx, col) in recent_request_columns(&text).into_iter().enumerate() {
            let width = col.width();
            if idx >= slot.len() {
                slot.push(width);
            } else {
                slot[idx] = slot[idx].max(width);
            }
        }
    }
    widths
}

fn normalize_recent_request_text(text: &str, widths: &[usize]) -> String {
    let text = format_recent_request_token_counts(text);
    let cols = recent_request_columns(&text);
    if cols.len() <= 1 {
        return text;
    }

    let mut out = String::new();
    for (idx, col) in cols.into_iter().enumerate() {
        if idx > 0 {
            out.push_str("  |  ");
        }
        let width = widths.get(idx).copied().unwrap_or_else(|| col.width());
        let normalized = truncate_to_width(col, width);
        out.push_str(&normalized);
        for _ in normalized.width()..width {
            out.push(' ');
        }
    }
    out
}

/// Format token counts in the response lines shown in Recent Requests.
///
/// Request metadata also contains numbers (HTTP status, elapsed seconds,
/// tool/message counts), so only numbers carrying the response token labels
/// are changed: `in`, `out`, `thinking`, and cache `r`/`w` counts.
fn format_recent_request_token_counts(text: &str) -> String {
    let mut columns = Vec::new();
    for column in recent_request_columns(text) {
        let mut column = column.to_owned();
        if column.contains("cache:") {
            column = format_token_counts_with_suffix(&column, "r");
            column = format_token_counts_with_suffix(&column, "w");
        } else {
            column = format_token_counts_with_suffix(&column, " in");
            column = format_token_counts_with_suffix(&column, " out");
            column = format_token_counts_with_suffix(&column, " thinking");
        }
        columns.push(column);
    }
    columns.join("  |  ")
}

/// Replace an ASCII number immediately before `suffix`, retaining all other
/// text byte-for-byte. The suffix boundary prevents matching a number that
/// is merely part of a larger word.
fn format_token_counts_with_suffix(text: &str, suffix: &str) -> String {
    let bytes = text.as_bytes();
    let mut replacements = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        let end = index;
        if !text[end..].starts_with(suffix) {
            continue;
        }
        let suffix_end = end + suffix.len();
        if suffix_end < bytes.len() && bytes[suffix_end].is_ascii_alphanumeric() {
            continue;
        }
        if start > 0 && bytes[start - 1].is_ascii_alphanumeric() {
            continue;
        }
        let Ok(count) = text[start..end].parse::<u64>() else {
            continue;
        };
        replacements.push((
            start,
            suffix_end,
            format!("{}{}", format_token_count(count), suffix),
        ));
    }

    let mut out = String::with_capacity(text.len());
    let mut previous = 0;
    for (start, end, replacement) in replacements {
        out.push_str(&text[previous..start]);
        out.push_str(&replacement);
        previous = end;
    }
    out.push_str(&text[previous..]);
    out
}

fn recent_request_columns(text: &str) -> Vec<&str> {
    text.split("  |  ").collect()
}

fn log_kind_index(kind: LogKind) -> usize {
    match kind {
        LogKind::Inbound => 0,
        LogKind::Response => 1,
        LogKind::Warning => 2,
        LogKind::Error => 3,
        LogKind::Info => 4,
    }
}

fn style_token_usage_line(text: &str) -> Line<'static> {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut index = 0;

    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }

        let start = index;
        while index < bytes.len() && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'.')
        {
            index += 1;
        }
        let end = index;
        let before = text[..start].trim_end();
        let after = &text[end..];
        let color =
            if before.ends_with("cache") || before.contains("cache:") || before.ends_with('/') {
                Some(TOKEN_CACHE_COLOR)
            } else if before.ends_with("in") || after.starts_with(" in") {
                Some(TOKEN_INPUT_COLOR)
            } else if before.ends_with("out") || after.starts_with(" out") {
                Some(TOKEN_OUTPUT_COLOR)
            } else if before.ends_with("reason")
                || before.ends_with("thinking")
                || after.starts_with(" thinking")
            {
                Some(TOKEN_REASONING_COLOR)
            } else {
                None
            };

        if let Some(color) = color {
            ranges.push((start, end, color));
        }
    }

    let mut spans = Vec::with_capacity(ranges.len() * 2 + 1);
    let mut cursor = 0;
    for (start, end, color) in ranges {
        spans.push(Span::raw(text[cursor..start].to_owned()));
        spans.push(Span::styled(
            text[start..end].to_owned(),
            Style::default().fg(color),
        ));
        cursor = end;
    }
    spans.push(Span::raw(text[cursor..].to_owned()));
    Line::from(spans)
}

/// Placeholder for a cell with nothing to show yet. An em dash reads as
/// "no data" where a zero would read as "zero tokens", which is a
/// different (and misleading) claim for a model that has not been used.
const DASH: &str = "\u{2014}";

/// Render one row of the merged routing + token table.
///
/// Columns are fixed-width so the per-model rows, the header and the
/// trailing summary rows all line up; `Panel::row` truncates the whole
/// line if the terminal is narrower than the table.
fn format_table_row(
    selected: bool,
    dirty: bool,
    index: usize,
    inbound: &str,
    upstream: &str,
    learned: &str,
    total: Option<&TokenTotals>,
) -> String {
    let cursor = if selected { '>' } else { ' ' };
    let dirty = if dirty { '*' } else { ' ' };
    let (req, input, output, thinking, cache) = total.map_or_else(
        || {
            (
                DASH.to_owned(),
                DASH.to_owned(),
                DASH.to_owned(),
                DASH.to_owned(),
                DASH.to_owned(),
            )
        },
        |t| {
            (
                t.requests.to_string(),
                format_token_count(t.input_tokens),
                format_token_count(t.output_tokens),
                format_token_count(t.reasoning_tokens),
                format_cache_counts(t),
            )
        },
    );
    format!(
        "  {cursor}{index:<2}{dirty} {inbound:<26} {upstream:<20} {learned:<12} {req:>5} {input:>7} {output:>7} {thinking:>7} {cache:>12}",
        inbound = truncate(inbound, 26),
        upstream = truncate(upstream, 20),
        learned = truncate(learned, 12),
    )
}

/// Render a trailing summary row. The label is padded into the space the
/// mapping columns occupy so the numbers land under the table's numeric
/// columns instead of drifting left.
fn format_summary_row(label: &str, total: &TokenTotals) -> String {
    format!(
        "  {:<66}{:>5} {:>7} {:>7} {:>7} {:>12}",
        truncate(label, 66),
        total.requests,
        format_token_count(total.input_tokens),
        format_token_count(total.output_tokens),
        format_token_count(total.reasoning_tokens),
        format_cache_counts(total),
    )
}

/// Prompt-cache traffic, compacted for the CACHE column as `{read}r/{write}w`.
fn format_cache_counts(total: &TokenTotals) -> String {
    format!(
        "{}r/{}w",
        format_token_count(total.cache_read_input_tokens),
        format_token_count(total.cache_creation_input_tokens)
    )
}

/// Compact rendering of the parameters learned to be unsupported for one
/// upstream model, for the LEARNED column.
fn format_learned_params(params: &[crate::capabilities::RequestParam]) -> String {
    if params.is_empty() {
        return DASH.to_owned();
    }
    params
        .iter()
        .map(|p| short_param_name(*p))
        .collect::<Vec<_>>()
        .join("+")
}

/// Abbreviate a parameter name so the LEARNED column stays narrow.
fn short_param_name(param: crate::capabilities::RequestParam) -> &'static str {
    match param {
        crate::capabilities::RequestParam::Temperature => "temp",
        crate::capabilities::RequestParam::TopP => "top_p",
    }
}

/// A rule with a label set into its left end, used to head the trailing
/// totals groups: `-- ALL COMBINED -----------------------`.
fn labelled_rule(label: &str, inner_width: usize) -> String {
    let prefix = format!("\u{2500}\u{2500} {label} ");
    let used = prefix.width();
    if used >= inner_width {
        return truncate_to_width(&prefix, inner_width);
    }
    format!("{prefix}{}", "\u{2500}".repeat(inner_width - used))
}

/// Grand total across both inbound and actual-fallback models.
fn combined_totals(sections: &SessionStatsSnapshot) -> TokenTotals {
    let mut combined = TokenTotals::default();
    for total in sections.inbound.values().chain(sections.actual.values()) {
        combined.add_totals(*total);
    }
    combined
}

/// Format a token count compactly for the dashboard.
///
/// Counts below one thousand remain integer counts so small values do not
/// gain needless decimal places. Larger counts use decimal (base-1000) units
/// and are rounded to two decimal places. If rounding would produce
/// `1000.00` of one unit, the count is promoted to the next unit instead.
fn format_token_count(count: u64) -> String {
    const UNITS: [&str; 6] = ["", "k", "m", "b", "t", "q"];

    if count < 1_000 {
        return count.to_string();
    }

    let mut unit_index = 0;
    let mut scaled = count as f64;
    while scaled >= 1_000.0 && unit_index + 1 < UNITS.len() {
        scaled /= 1_000.0;
        unit_index += 1;
    }

    // Avoid output such as `1000.00k` when rounding carries into the next
    // unit. This also keeps values near a unit boundary intuitive.
    while scaled >= 999.995 && unit_index + 1 < UNITS.len() {
        scaled /= 1_000.0;
        unit_index += 1;
    }

    format!("{scaled:.2}{}", UNITS[unit_index])
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn prev_char_boundary(s: &str, idx: usize) -> usize {
    if idx == 0 {
        return 0;
    }
    let mut i = idx - 1;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx + 1;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i.min(s.len())
}

#[cfg(test)]
mod tests;
