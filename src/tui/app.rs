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
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use super::output::LogLine;
use super::panels::Panel;
use super::runtime::MappingsStore;

/// Maximum number of log lines retained for the on-screen tail.
const LOG_TAIL: usize = 32;

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
    /// Recent log lines, newest at the back.
    log: VecDeque<LogLine>,
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
    pub fn new(store: std::sync::Arc<MappingsStore>, config_path: Option<PathBuf>) -> Self {
        Self {
            store,
            config_path,
            log: VecDeque::with_capacity(LOG_TAIL),
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
        let live: super::runtime::RuntimeMappings =
            (*self.store.load_live()).as_ref().clone();
        let json = match serde_json::to_string_pretty(&live) {
            Ok(j) => j,
            Err(e) => {
                self.toast = Some(format!("save failed: {e}"));
                self.mode = Mode::SavedToast;
                return;
            }
        };
        // Write to a temp file, then atomic-rename. This avoids leaving
        // a half-written proxy.json if the process is killed mid-write.
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, &json) {
            self.toast = Some(format!("save failed: {e}"));
            self.mode = Mode::SavedToast;
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            // Best-effort: try to clean up the temp file.
            let _ = std::fs::remove_file(&tmp);
            self.toast = Some(format!("save failed: {e}"));
            self.mode = Mode::SavedToast;
            return;
        }
        self.store.mark_saved();
        self.toast = Some(format!("saved {path:?}"));
        self.mode = Mode::SavedToast;
    }

    /// Render the TUI. `area` is the full terminal area.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        // Width budget. We use 4 cells of margin so panels don't
        // press against the terminal edge, and cap at the terminal
        // width minus the margin.
        let margin: u16 = 2;
        let max_w = area.width.saturating_sub(margin * 2);
        let inner_w = max_w.saturating_sub(2).max(20);

        // ---- Dashboard panel ----
        let snap = self.store.snapshot();
        let mut dash = Panel::new(
            format!(
                "openai-to-anthropic proxy  |  v0.1.0  |  model routing console  |  uptime {}s",
                self.started_at.elapsed().as_secs()
            ),
            inner_w,
        );
        dash.row(&format!(
            "STATUS  ONLINE       LISTEN  127.0.0.1:8080       UPSTREAM  CONNECTED       CONFIG  {}",
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

        // ---- Log panel ----
        let mut log_panel = Panel::new("RECENT REQUESTS  (oldest first)", inner_w);
        if self.log.is_empty() {
            log_panel.row("  (no requests yet)");
        } else {
            for line in self.log.iter() {
                log_panel.row(&format!("  {}", line.text));
            }
        }

        // ---- Layout ----
        // We render two stacked panels using ratatui's Layout: the
        // dashboard on top, the log below. Each panel's height equals
        // its own line count, so they always fit.
        let dash_h = dash.len() as u16;
        let log_h = log_panel.len() as u16;
        let vchunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(dash_h),
                Constraint::Length(1), // gap
                Constraint::Length(log_h),
            ])
            .split(area);

        // Center each panel horizontally.
        let centered = |h: u16, parent: Rect| -> Rect {
            let x = parent.x + (parent.width.saturating_sub(h)) / 2;
            Rect::new(x, parent.y, h, parent.width)
        };

        frame.render_widget(
            Paragraph::new(dash.lines.join("\n"))
                .style(Style::default().fg(Color::White)),
            centered(dash.width, vchunks[0]),
        );
        frame.render_widget(
            Paragraph::new(log_panel.lines.join("\n"))
                .style(Style::default().fg(Color::Gray)),
            centered(log_panel.width, vchunks[2]),
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

    #[test]
    fn rows_are_sorted() {
        let app = TuiApp::new(make_store(), None);
        let rows = app.rows();
        assert_eq!(rows[0].inbound, "claude-opus-4-8");
        assert_eq!(rows[1].inbound, "claude-sonnet-5");
    }

    #[test]
    fn add_then_dirty() {
        let mut app = TuiApp::new(make_store(), None);
        app.apply_mutation(|m| {
            m.map
                .insert("claude-haiku-4-5".into(), "gpt-4o-mini".into());
        });
        assert!(app.store.snapshot().is_dirty());
    }

    #[test]
    fn delete_clears_mapping() {
        let mut app = TuiApp::new(make_store(), None);
        app.selected = 0; // claude-opus-4-8
        app.delete_selected();
        let rows = app.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].inbound, "claude-sonnet-5");
    }

    #[test]
    fn save_disabled_when_no_path() {
        let mut app = TuiApp::new(make_store(), None);
        app.save_to_disk();
        assert!(app.toast.is_some());
    }

}
