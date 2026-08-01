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
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};
use unicode_width::UnicodeWidthStr;

use super::output::{LogKind, LogLine};
use super::panels::Panel;
use super::runtime::{MappingsStore, RuntimeMappings};
use crate::config::JsonConfig;

/// Maximum number of log lines retained for the on-screen tail.
///
/// Raised from 32 to 1000 so the tail is useful in practice. With
/// the scrollable `List` widget the entire buffer is reachable via
/// `PageUp`/`PageDown`/`Home`/`End`.
const LOG_TAIL: usize = 1000;

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
}

impl TuiApp {
    /// Construct a new app.
    pub fn new(
        store: std::sync::Arc<MappingsStore>,
        config_path: Option<PathBuf>,
        listen_addr: String,
        upstream_base_url: String,
    ) -> Self {
        let mut log_list_state = ListState::default();
        // Show the newest entry at the top by default (offset 0).
        log_list_state.select(Some(0));
        Self {
            store,
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
                false
            }
        }
    }

    fn on_key_normal(&mut self, key: KeyEvent) -> bool {
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
    }

    /// Jump to the newest log entry (offset 0). Re-enables tail-follow.
    fn log_jump_to_newest(&mut self) {
        if self.log.is_empty() {
            return;
        }
        *self.log_list_state.offset_mut() = 0;
        self.log_list_state.select(Some(0));
        self.log_follow_tail = true;
    }

    fn on_key_editing(&mut self, key: KeyEvent, field: EditField) -> bool {
        // Within the editor:
        //   Tab toggles the focused field.
        //   Enter applies the change (in-memory).
        //   Esc cancels.
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
        self.edit_outbound = snap
            .live
            .default_model
            .clone()
            .unwrap_or_default();
        self.edit_cursor = self.edit_outbound.len();
        self.mode = Mode::Editing(EditField::Outbound);
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
    }

    fn apply_mutation(&self, f: impl FnOnce(&mut super::runtime::RuntimeMappings)) {
        // Read the current live snapshot, mutate a clone, and swap.
        let mut next: super::runtime::RuntimeMappings =
            (*self.store.load_live()).as_ref().clone();
        f(&mut next);
        self.store.set_live(next);
    }

    fn save_to_disk(&mut self) {
        let Some(path) = self.config_path.as_ref() else {
            self.toast = Some("save disabled: --no-config or no proxy.json".into());
            self.mode = Mode::SavedToast;
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
                        return;
                    }
                },
                Err(e) => {
                    self.toast = Some(format!("save failed: read error: {e}"));
                    self.mode = Mode::SavedToast;
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
                return;
            }
        };
        
        // Write to a temp file, then atomic-rename. This avoids leaving
        // a half-written proxy.json if the process is killed mid-write.
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, &json) {
            self.toast = Some(format!("save failed: write error: {e}"));
            self.mode = Mode::SavedToast;
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            // Best-effort: try to clean up the temp file.
            let _ = std::fs::remove_file(&tmp);
            self.toast = Some(format!("save failed: rename error: {e}"));
            self.mode = Mode::SavedToast;
            return;
        }
        self.store.mark_saved();
        self.toast = Some(format!("saved {path:?}"));
        self.mode = Mode::SavedToast;
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
        dash.row(&format!(
            "STATUS  ONLINE   LISTEN  {:<21}   UPSTREAM  {}   CONFIG  {}",
            self.listen_addr,
            self.upstream_base_url,
            self.config_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<none>".into())
        ));
        dash.row(&format!(
            "MAPPINGS  {}          UNSAVED CHANGES  {}             PRESS  q TO QUIT",
            snap.live.map.len(),
            if snap.is_dirty() { "1" } else { "0" }
        ));
        dash.rule();

        // ---- Mappings panel ----
        dash.row("MODEL MAPPINGS  (use j/k to select, Enter to edit, a/d to add/delete)");
        dash.row("  #   INBOUND MODEL             UPSTREAM MODEL             STATE");
        for (i, row) in self.rows().iter().enumerate() {
            let marker = if row.dirty { " * " } else { "   " };
            let cursor = if i == self.selected { "> " } else { "  " };
            let line = format!(
                "  {}{}{:<26}{:<30}{}",
                cursor,
                marker,
                truncate(&row.inbound, 26),
                truncate(&row.outbound, 30),
                "ACTIVE"
            );
            dash.row(&line);
        }
        dash.rule();
        let fb = snap
            .live
            .default_model
            .as_deref()
            .unwrap_or("<none>");
        dash.row(&format!(
            "DEFAULT FALLBACK  {fb}        SAVE STATUS  {}",
            if snap.is_dirty() { "UNSAVED  (press S to save)" } else { "SAVED" }
        ));

        // ---- Footer hint ----
        dash.row("[a] add  [e/Enter] edit  [d] delete  [f] default  [s] save  [q] quit");
        dash.row("[PgUp/PgDn] scroll log  [Home/End] jump log top/bottom");

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
                Constraint::Min(3),            // log: at least 3 rows
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
        frame.render_widget(
            Paragraph::new(dash.lines.join("\n"))
                .style(Style::default().fg(Color::White)),
            centered(dash.width, dash_area),
        );

        // ---- Log (scrollable List + Scrollbar) ----
        // Build items newest-first. The `List` widget's `offset`
        // refers to the first visible item; with newest at index 0
        // and offset 0 the newest line is at the top.
        let items: Vec<ListItem> = self
            .log
            .iter()
            .rev()
            .map(|line| ListItem::new(format!("{}  {}", kind_glyph(line.kind), line.text)))
            .collect();
        let title = if self.log.is_empty() {
            "RECENT REQUESTS  (no requests yet)  [PgUp/PgDn to scroll]"
        } else {
            "RECENT REQUESTS  (newest first)  [PgUp/PgDn, Home/End to scroll]"
        };
        let list = List::new(items)
            .block(Block::bordered().title(title))
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
            let mut sb_state = ScrollbarState::new(self.log.len())
                .position(self.log_list_state.offset());
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
                "[a] add  [e] edit  [d] delete  [f] default  [s] save  [q] quit   |   log: {tail_note}   |   PgUp/PgDn scroll"
            ),
            area.width.saturating_sub(2) as usize,
        );
        frame.render_widget(
            Paragraph::new(footer_text.as_str())
                .style(Style::default().fg(Color::DarkGray)),
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
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let outbound_style = if field == EditField::Outbound {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
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
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn make_store() -> std::sync::Arc<MappingsStore> {
        std::sync::Arc::new(MappingsStore::from_parts(
            super::super::runtime::RuntimeMappings {
                map: [
                    ("claude-opus-4-8".to_owned(), "gpt-5.6-luna".to_owned()),
                    ("claude-sonnet-5".to_owned(), "gpt-5.4-mini".to_owned()),
                ]
                .into_iter()
                .collect(),
                default_model: Some("gpt-4.1".into()),
            },
            super::super::runtime::RuntimeMappings::default(),
        ))
    }

    fn make_log_line(text: &str) -> LogLine {
        LogLine {
            text: text.to_owned(),
            kind: LogKind::Info,
        }
    }

    #[test]
    fn rows_are_sorted() {
        let app = TuiApp::new(make_store(), None, "0.0.0.0:8085".into(), "http://localhost/v1".into());
        let rows = app.rows();
        assert_eq!(rows[0].inbound, "claude-opus-4-8");
        assert_eq!(rows[1].inbound, "claude-sonnet-5");
    }

    #[test]
    fn add_then_dirty() {
        let mut app = TuiApp::new(make_store(), None, "0.0.0.0:8085".into(), "http://localhost/v1".into());
        app.apply_mutation(|m| {
            m.map
                .insert("claude-haiku-4-5".into(), "gpt-4o-mini".into());
        });
        assert!(app.store.snapshot().is_dirty());
    }

    #[test]
    fn delete_clears_mapping() {
        let mut app = TuiApp::new(make_store(), None, "0.0.0.0:8085".into(), "http://localhost/v1".into());
        app.selected = 0; // claude-opus-4-8
        app.delete_selected();
        let rows = app.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].inbound, "claude-sonnet-5");
    }

    #[test]
    fn save_disabled_when_no_path() {
        let mut app = TuiApp::new(make_store(), None, "0.0.0.0:8085".into(), "http://localhost/v1".into());
        app.save_to_disk();
        assert!(app.toast.is_some());
    }

    /// REGRESSION TEST for the ratatui panic
    /// `index outside of buffer: the area is Rect { x: 0, y: 0,
    /// width: 120, height: 30 } but index is (2, 30)`.
    ///
    /// On the pre-fix code, the `centered` closure passed
    /// `parent.width` as the last argument to `Rect::new`, so the
    /// dashboard Paragraph was given a 120-row area on a 30-row
    /// terminal. As soon as the log Paragraph was rendered below
    /// it, the cursor ran off the bottom of the buffer and the
    /// rendering thread panicked.
    ///
    /// On the fixed code, the layout is content-size-safe: the
    /// dashboard height is capped at `area.height - reserved`, and
    /// the log is rendered as a scrollable `List` that can never
    /// overflow. This test fills the log to its `LOG_TAIL` cap and
    /// renders into a 120x30 `TestBackend`. Pre-fix, this would
    /// panic. Post-fix, it returns `Ok`.
    #[test]
    fn render_does_not_overflow_buffer_when_log_is_full() {
        let mut app = TuiApp::new(make_store(), None, "0.0.0.0:8085".into(), "http://localhost/v1".into());
        // Fill the log well past what a 30-row terminal can show.
        for i in 0..(LOG_TAIL + 50) {
            app.push_log(make_log_line(&format!("request #{i} inbound foo -> bar")));
        }
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        // The panic happens inside the draw closure, so we wrap
        // the closure body in `catch_unwind` and return the result
        // out via a `Cell`.
        let outcome = std::cell::Cell::new(None::<std::thread::Result<()>>);
        terminal
            .draw(|frame| {
                let area = frame.area();
                outcome.set(Some(std::panic::catch_unwind(
                    std::panic::AssertUnwindSafe(|| {
                        app.render(frame, area);
                    }),
                )));
            })
            .expect("draw failed");
        let result = outcome
            .take()
            .expect("draw closure did not run");
        // The draw must complete without panicking.
        assert!(
            result.is_ok(),
            "render panicked with full log: {result:?}"
        );
    }

    /// At a minimum terminal height (5 rows) the layout must still
    /// fit. The dashboard is capped and the log is given its
    /// reserved 3-row minimum.
    #[test]
    fn render_does_not_overflow_buffer_at_minimum_height() {
        let mut app = TuiApp::new(make_store(), None, "0.0.0.0:8085".into(), "http://localhost/v1".into());
        for i in 0..200 {
            app.push_log(make_log_line(&format!("req {i}")));
        }
        let backend = TestBackend::new(80, 5);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let outcome = std::cell::Cell::new(None::<std::thread::Result<()>>);
        terminal
            .draw(|frame| {
                let area = frame.area();
                outcome.set(Some(std::panic::catch_unwind(
                    std::panic::AssertUnwindSafe(|| {
                        app.render(frame, area);
                    }),
                )));
            })
            .expect("draw failed");
        let result = outcome.take().expect("draw closure did not run");
        assert!(result.is_ok(), "render panicked at 80x5: {result:?}");
    }

    /// Even with no log lines, a fat mappings list must not
    /// overflow. This exercises the dashboard-cap path.
    #[test]
    fn render_does_not_overflow_buffer_with_many_mappings() {
        let store = MappingsStore::from_parts(
            super::super::runtime::RuntimeMappings::default(),
            super::super::runtime::RuntimeMappings::default(),
        );
        {
            let mut m: super::super::runtime::RuntimeMappings =
                (*store.load_live()).as_ref().clone();
            for i in 0..50 {
                m.map.insert(format!("claude-in-{i}"), format!("gpt-out-{i}"));
            }
            store.set_live(m);
        }
        let mut app = TuiApp::new(std::sync::Arc::new(store), None, "0.0.0.0:8085".into(), "http://localhost/v1".into());
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let outcome = std::cell::Cell::new(None::<std::thread::Result<()>>);
        terminal
            .draw(|frame| {
                let area = frame.area();
                outcome.set(Some(std::panic::catch_unwind(
                    std::panic::AssertUnwindSafe(|| {
                        app.render(frame, area);
                    }),
                )));
            })
            .expect("draw failed");
        let result = outcome.take().expect("draw closure did not run");
        assert!(
            result.is_ok(),
            "render panicked with 50 mappings: {result:?}"
        );
    }

    /// The `LOG_TAIL` cap is enforced: pushing more than `LOG_TAIL`
    /// lines keeps the deque at exactly `LOG_TAIL` entries.
    #[test]
    fn log_cap_respected() {
        let mut app = TuiApp::new(make_store(), None, "0.0.0.0:8085".into(), "http://localhost/v1".into());
        for i in 0..(LOG_TAIL * 2) {
            app.push_log(make_log_line(&format!("line {i}")));
        }
        assert_eq!(app.log.len(), LOG_TAIL);
    }

    /// The log scroll handlers must keep `ListState.offset` in
    /// the valid range `[0, log.len().saturating_sub(1)]`, no
    /// matter what direction the user scrolls.
    #[test]
    fn scroll_offset_clamps_to_valid_range() {
        let mut app = TuiApp::new(make_store(), None, "0.0.0.0:8085".into(), "http://localhost/v1".into());
        // Push enough lines to make the offset non-trivial.
        for i in 0..200 {
            app.push_log(make_log_line(&format!("line {i}")));
        }
        // Scroll up well past the top.
        for _ in 0..1000 {
            app.log_scroll_up(10);
        }
        let max_offset = app.log.len().saturating_sub(1);
        assert_eq!(app.log_list_state.offset(), max_offset);

        // Scroll down well past the bottom.
        for _ in 0..1000 {
            app.log_scroll_down(10);
        }
        assert_eq!(app.log_list_state.offset(), 0);
        assert!(app.log_follow_tail, "scroll to top re-enables tail-follow");

        // Jump to oldest, then to newest.
        app.log_jump_to_oldest();
        assert_eq!(app.log_list_state.offset(), max_offset);
        assert!(!app.log_follow_tail);
        app.log_jump_to_newest();
        assert_eq!(app.log_list_state.offset(), 0);
        assert!(app.log_follow_tail);
    }

    /// `centered` must produce a `Rect` whose height equals
    /// `parent.height`. Pre-fix, the height was `parent.width`,
    /// which is what caused the original panic.
    #[test]
    fn centered_preserves_height() {
        let parent = Rect::new(0, 0, 120, 30);
        let r = centered(80, parent);
        assert_eq!(r.height, 30, "height must be parent.height");
        assert_eq!(r.width, 80);
        // Centred horizontally: x is at the midpoint of the slack.
        assert_eq!(r.x, (120 - 80) / 2);
    }

    /// `centered` must clamp the panel width to `parent.width` so a
    /// too-wide request never overflows the parent buffer.
    #[test]
    fn centered_handles_wider_than_parent() {
        let parent = Rect::new(0, 0, 40, 20);
        let r = centered(80, parent);
        // Saturating arithmetic clamps x to parent.x.
        assert_eq!(r.x, 0);
        assert_eq!(r.height, 20);
        // Width is clamped to parent.width.
        assert_eq!(r.width, 40);
    }
}
