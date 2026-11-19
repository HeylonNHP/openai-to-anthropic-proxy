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
fn token_usage_values_are_color_coded() {
    let line = style_token_usage_line(
        "  model                           7 req  in      1.00k  out      1.00m  cache      1.23k/999  reason      1.00b",
    );
    let styled: Vec<_> = line
        .spans
        .iter()
        .filter(|span| span.style.fg.is_some())
        .collect();

    assert_eq!(styled.len(), 5);
    assert_eq!(styled[0].content, "1.00k");
    assert_eq!(styled[0].style.fg, Some(TOKEN_INPUT_COLOR));
    assert_eq!(styled[1].content, "1.00m");
    assert_eq!(styled[1].style.fg, Some(TOKEN_OUTPUT_COLOR));
    assert_eq!(styled[2].content, "1.23k");
    assert_eq!(styled[2].style.fg, Some(TOKEN_CACHE_COLOR));
    assert_eq!(styled[3].content, "999");
    assert_eq!(styled[3].style.fg, Some(TOKEN_CACHE_COLOR));
    assert_eq!(styled[4].content, "1.00b");
    assert_eq!(styled[4].style.fg, Some(TOKEN_REASONING_COLOR));
}

#[test]
fn recent_request_token_values_are_color_coded() {
    let text = format_recent_request_token_counts(
        "  200  |  1.25s  |  1234 in  |  999995 out  |  2000000 thinking  |  cache: 1000000r 999w",
    );
    let line = style_token_usage_line(&text);
    let styled: Vec<_> = line
        .spans
        .iter()
        .filter(|span| span.style.fg.is_some())
        .collect();

    assert_eq!(styled.len(), 5);
    assert_eq!(styled[0].content, "1.23k");
    assert_eq!(styled[0].style.fg, Some(TOKEN_INPUT_COLOR));
    assert_eq!(styled[1].content, "1.00m");
    assert_eq!(styled[1].style.fg, Some(TOKEN_OUTPUT_COLOR));
    assert_eq!(styled[2].content, "2.00m");
    assert_eq!(styled[2].style.fg, Some(TOKEN_REASONING_COLOR));
    assert_eq!(styled[3].content, "1.00mr");
    assert_eq!(styled[3].style.fg, Some(TOKEN_CACHE_COLOR));
    assert_eq!(styled[4].content, "999w");
    assert_eq!(styled[4].style.fg, Some(TOKEN_CACHE_COLOR));
}

/// A tool row carries URLs, queries and error codes. A number in one of
/// those is not a token count, so the token formatting must leave it alone.
#[test]
fn tool_rows_keep_their_numbers_as_they_are() {
    let lines = vec![LogLine {
        text: "  web_search  |  \"1200 in stock\"  |  ok  |  1.00s  |  2 results".into(),
        kind: LogKind::Tool,
    }];
    let widths = recent_request_column_widths(lines.iter());
    let normalized = normalize_recent_request_text(
        LogKind::Tool,
        &lines[0].text,
        &widths[log_kind_index(LogKind::Tool)],
    );
    assert!(normalized.contains("1200 in stock"), "{normalized}");
}

#[test]
fn a_tool_rows_outcome_is_coloured_by_what_happened() {
    let worked =
        style_tool_line("  web_fetch  |  https://example.com/  |  ok  |  1.00s  |  2 chars");
    let coloured: Vec<_> = worked
        .spans
        .iter()
        .filter(|span| span.style.fg.is_some())
        .collect();
    assert_eq!(coloured.len(), 2, "the outcome and the elapsed time");
    assert_eq!(coloured[0].content, "ok");
    assert_eq!(coloured[0].style.fg, Some(TOOL_OK_COLOR));
    assert_eq!(coloured[1].content, "1.00s");
    assert_eq!(coloured[1].style.fg, Some(TOOL_DIM_COLOR));

    let failed =
        style_tool_line("  web_fetch  |  https://intranet.corp/  |  error  |  0.02s  |  —");
    let coloured: Vec<_> = failed
        .spans
        .iter()
        .filter(|span| span.style.fg.is_some())
        .collect();
    assert_eq!(coloured[0].content, "error");
    assert_eq!(coloured[0].style.fg, Some(TOOL_ERROR_COLOR));
}

#[test]
fn tool_rows_are_rendered_in_the_log_pane() {
    let mut app = TuiApp::new(
        make_store(),
        None,
        "0.0.0.0:8085".into(),
        "http://localhost/v1".into(),
    );
    app.push_log(LogLine {
        text: "  web_fetch  |  https://example.com/docs  |  ok  |  1.24s  |  18.43k chars".into(),
        kind: LogKind::Tool,
    });

    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            let area = frame.area();
            app.render(frame, area);
        })
        .expect("draw failed");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .chunks(120)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(rendered.contains("web_fetch"), "{rendered}");
    assert!(rendered.contains("https://example.com/docs"), "{rendered}");
    assert!(rendered.contains("18.43k chars"), "{rendered}");
}

#[test]
fn summary_row_formats_every_token_field() {
    let total = TokenTotals {
        requests: 7,
        input_tokens: 1_000,
        output_tokens: 1_000_000,
        cache_read_input_tokens: 1_234,
        cache_creation_input_tokens: 999,
        reasoning_tokens: 1_000_000_000,
    };

    let row = format_summary_row("model", &total);
    assert!(row.contains("1.00k"), "input: {row}");
    assert!(row.contains("1.00m"), "output: {row}");
    assert!(row.contains("1.23kr/999w"), "cache: {row}");
    assert!(row.contains("1.00b"), "reasoning: {row}");
    // The label must not change the row width, or columns drift.
    assert_eq!(
        row.len(),
        format_summary_row("a much longer label here", &total).len()
    );
}

/// The table row, the header and the summary rows must all be the
/// same width, or the columns visibly drift apart.
#[test]
fn merged_table_rows_are_all_the_same_width() {
    let header = format!(
        "  {:<5}{:<26} {:<20} {:<12} {:>5} {:>7} {:>7} {:>7} {:>12}",
        "#", "INBOUND MODEL", "UPSTREAM MODEL", "LEARNED", "REQ", "IN", "OUT", "THINK", "CACHE"
    );
    let total = TokenTotals {
        requests: 42,
        input_tokens: 1_200_000,
        output_tokens: 340_000,
        cache_read_input_tokens: 1_000_000,
        cache_creation_input_tokens: 0,
        reasoning_tokens: 900_000,
    };
    let populated = format_table_row(
        true,
        true,
        7,
        "claude-opus-4-8",
        "gpt-5.6-terra",
        "temp+top_p",
        Some(&total),
    );
    let empty = format_table_row(false, false, 8, "haiku", "gpt-5.6-luna", DASH, None);
    let summary = format_summary_row("all models", &total);

    for (name, line) in [
        ("header", header),
        ("populated", populated),
        ("empty", empty),
        ("summary", summary),
    ] {
        // Compare cells, not bytes: the em dash is three bytes but
        // one column, and the empty row is full of them.
        assert_eq!(
            line.chars().count(),
            110,
            "{name} row width drifted: {line}"
        );
    }
}

/// A model with no traffic yet shows em dashes rather than zeros, so
/// "not used" is never mistaken for "used zero tokens".
#[test]
fn unused_model_row_shows_dashes_not_zeros() {
    let row = format_table_row(false, false, 1, "haiku", "gpt-5.6-luna", DASH, None);
    assert!(
        !row.contains('0'),
        "no zero counts for an unused model: {row}"
    );
    // One for LEARNED plus one per token column.
    assert_eq!(row.matches(DASH).count(), 6, "one dash per empty cell");
}

#[test]
fn learned_params_are_abbreviated_for_the_table() {
    use crate::capabilities::RequestParam;
    assert_eq!(format_learned_params(&[]), DASH);
    assert_eq!(format_learned_params(&[RequestParam::Temperature]), "temp");
    assert_eq!(
        format_learned_params(&[RequestParam::Temperature, RequestParam::TopP]),
        "temp+top_p"
    );
}

#[test]
fn labelled_rule_fills_the_inner_width() {
    let rule = labelled_rule("ALL COMBINED", 112);
    assert_eq!(rule.width(), 112);
    assert!(rule.starts_with("\u{2500}\u{2500} ALL COMBINED "));
}

#[test]
fn recent_request_token_counts_use_compact_units_without_changing_metadata() {
    let text =
        "  200  |  1.25s  |  1234 in  |  999995 out  |  2000000 thinking  |  cache: 1000000r 999w";

    assert_eq!(
        format_recent_request_token_counts(text),
        "  200  |  1.25s  |  1.23k in  |  1.00m out  |  2.00m thinking  |  cache: 1.00mr 999w"
    );
}

#[test]
fn recent_request_normalization_aligns_formatted_token_counts() {
    let lines = vec![
        LogLine {
            text: "  200  |  1.25s  |  1234 in  |  2 out  |  cache: 1000000r 999w".into(),
            kind: LogKind::Response,
        },
        LogLine {
            text: "  200  |  1.25s  |  2 in  |  999995 out  |  cache: 3r 1000000w".into(),
            kind: LogKind::Response,
        },
    ];
    let widths = recent_request_column_widths(lines.iter());
    let first = normalize_recent_request_text(LogKind::Response, &lines[0].text, &widths[1]);
    let second = normalize_recent_request_text(LogKind::Response, &lines[1].text, &widths[1]);

    assert!(first.contains("1.23k in"));
    assert!(first.contains("1.00mr"));
    assert!(second.contains("1.00m out"));
    assert!(second.contains("1.00mw"));
    assert_eq!(
        first.find("|  1.23k in"),
        second.find("|  2 in"),
        "input token columns should retain alignment after formatting"
    );
}

#[test]
fn recent_request_rendering_shows_formatted_token_counts() {
    let mut app = TuiApp::new(
        make_store(),
        None,
        "0.0.0.0:8085".into(),
        "http://localhost/v1".into(),
    );
    app.push_log(LogLine {
            text: "  200  |  1.25s  |  1234 in  |  999995 out  |  2000000 thinking  |  cache: 1000000r 999w".into(),
            kind: LogKind::Response,
        });

    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            let area = frame.area();
            app.render(frame, area);
        })
        .expect("draw failed");
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .chunks(120)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\\n");

    assert!(rendered.contains("1.23k in"));
    assert!(rendered.contains("1.00m out"));
    assert!(rendered.contains("2.00m thinking"));
    assert!(rendered.contains("1.00mr 999w"));
}

#[test]
fn recent_request_columns_are_width_normalized() {
    let lines = vec![
            LogLine {
                text: "  → claude-sonnet-5 → gpt-5.4-mini  |  2 tools  |  12 messages  |  stream  |  reasoning: high".into(),
                kind: LogKind::Inbound,
            },
            LogLine {
                text: "  → claude-opus-4-8 → gpt-5.6-luna  |  12 tools  |  2 messages  |  stream  |  reasoning: high".into(),
                kind: LogKind::Inbound,
            },
        ];
    let widths = recent_request_column_widths(lines.iter());
    let normalized = normalize_recent_request_text(
        LogKind::Inbound,
        "  → claude-sonnet-5 → gpt-5.4-mini  |  2 tools  |  12 messages  |  stream  |  reasoning: high",
        &widths[log_kind_index(LogKind::Inbound)],
    );
    assert!(normalized.contains("|  12 messages"));
    assert!(normalized.contains("|  reasoning: high"));
}

#[test]
fn recent_request_columns_handle_wide_text() {
    let lines = vec![LogLine {
        text: "  → haiku → 繁體中文  |  12 tools  |  9 messages".into(),
        kind: LogKind::Inbound,
    }];
    let widths = recent_request_column_widths(lines.iter());
    let normalized = normalize_recent_request_text(
        LogKind::Inbound,
        "  → haiku → 繁體中文  |  12 tools  |  9 messages",
        &widths[log_kind_index(LogKind::Inbound)],
    );
    assert!(normalized.contains("繁體中文"));
    assert!(normalized.width() >= normalized.chars().count());
}

#[test]
fn rows_are_sorted() {
    let app = TuiApp::new(
        make_store(),
        None,
        "0.0.0.0:8085".into(),
        "http://localhost/v1".into(),
    );
    let rows = app.rows();
    assert_eq!(rows[0].inbound, "claude-opus-4-8");
    assert_eq!(rows[1].inbound, "claude-sonnet-5");
}

#[test]
fn add_then_dirty() {
    // `apply_mutation` now flips the TUI dirty flag, which
    // requires `&mut self` (so the runner's redraw gate sees
    // the update).
    let mut app = TuiApp::new(
        make_store(),
        None,
        "0.0.0.0:8085".into(),
        "http://localhost/v1".into(),
    );
    app.apply_mutation(|m| {
        m.map
            .insert("claude-haiku-4-5".into(), "gpt-4o-mini".into());
    });
    assert!(app.store.snapshot().is_dirty());
}

#[test]
fn delete_clears_mapping() {
    let mut app = TuiApp::new(
        make_store(),
        None,
        "0.0.0.0:8085".into(),
        "http://localhost/v1".into(),
    );
    app.selected = 0; // claude-opus-4-8
    app.delete_selected();
    let rows = app.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].inbound, "claude-sonnet-5");
}

#[test]
fn save_disabled_when_no_path() {
    let mut app = TuiApp::new(
        make_store(),
        None,
        "0.0.0.0:8085".into(),
        "http://localhost/v1".into(),
    );
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
    let mut app = TuiApp::new(
        make_store(),
        None,
        "0.0.0.0:8085".into(),
        "http://localhost/v1".into(),
    );
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
    let result = outcome.take().expect("draw closure did not run");
    // The draw must complete without panicking.
    assert!(result.is_ok(), "render panicked with full log: {result:?}");
}

/// At a minimum terminal height (5 rows) the layout must still
/// fit. The dashboard is capped and the log is given its
/// reserved 3-row minimum.
#[test]
fn render_does_not_overflow_buffer_at_minimum_height() {
    let mut app = TuiApp::new(
        make_store(),
        None,
        "0.0.0.0:8085".into(),
        "http://localhost/v1".into(),
    );
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
        let mut m: super::super::runtime::RuntimeMappings = (*store.load_live()).as_ref().clone();
        for i in 0..50 {
            m.map
                .insert(format!("claude-in-{i}"), format!("gpt-out-{i}"));
        }
        store.set_live(m);
    }
    let mut app = TuiApp::new(
        std::sync::Arc::new(store),
        None,
        "0.0.0.0:8085".into(),
        "http://localhost/v1".into(),
    );
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
    let mut app = TuiApp::new(
        make_store(),
        None,
        "0.0.0.0:8085".into(),
        "http://localhost/v1".into(),
    );
    for i in 0..(LOG_TAIL * 2) {
        app.push_log(make_log_line(&format!("line {i}")));
    }
    assert_eq!(app.log.len(), LOG_TAIL);
}

/// A freshly-constructed app must mark itself dirty so the
/// runner draws the very first frame (initial title, empty log,
/// "0 mappings" status row).
#[test]
fn new_app_is_initially_dirty() {
    let app = TuiApp::new(
        make_store(),
        None,
        "0.0.0.0:8085".into(),
        "http://localhost/v1".into(),
    );
    assert!(app.dirty, "new app should need a first-frame draw");
}

/// `take_dirty` returns the current value, then clears the flag.
/// A second consecutive call must return `false` so the runner
/// doesn't redraw twice on the same state.
#[test]
fn take_dirty_is_edge_triggered() {
    let mut app = TuiApp::new(
        make_store(),
        None,
        "0.0.0.0:8085".into(),
        "http://localhost/v1".into(),
    );
    // First call observes the initial dirty state and clears it.
    assert!(app.take_dirty());
    // Second call must observe a clean state — no further
    // mutation has happened.
    assert!(!app.take_dirty());
}

/// Every code path that mutates visible state must mark the app
/// dirty so the next runner iteration issues a `terminal.draw`.
/// This test pins the contract by clearing the flag (simulating
/// "we just drew") and verifying that each mutation flips it
/// back on.
#[test]
fn mutations_mark_dirty() {
    let mut app = TuiApp::new(
        make_store(),
        None,
        "0.0.0.0:8085".into(),
        "http://localhost/v1".into(),
    );
    // Drain the initial dirty bit so each subsequent assertion
    // observes the effect of the mutation alone.
    let _ = app.take_dirty();

    // Mappings mutations (apply_mutation, delete_selected).
    app.apply_mutation(|m| {
        m.map
            .insert("claude-haiku-4-5".into(), "gpt-4o-mini".into());
    });
    assert!(app.dirty, "apply_mutation must mark dirty");
    let _ = app.take_dirty();
    app.selected = 0;
    app.delete_selected();
    assert!(app.dirty, "delete_selected must mark dirty");
    let _ = app.take_dirty();

    // Log scroll handlers.
    for i in 0..5 {
        app.push_log(make_log_line(&format!("line {i}")));
    }
    let _ = app.take_dirty();
    app.log_scroll_down(1);
    assert!(app.dirty, "log_scroll_down must mark dirty");
    let _ = app.take_dirty();
    app.log_scroll_up(1);
    assert!(app.dirty, "log_scroll_up must mark dirty");
    let _ = app.take_dirty();
    app.log_jump_to_oldest();
    assert!(app.dirty, "log_jump_to_oldest must mark dirty");
    let _ = app.take_dirty();
    app.log_jump_to_newest();
    assert!(app.dirty, "log_jump_to_newest must mark dirty");
    let _ = app.take_dirty();

    // Mode transitions via the public on_key entry point.
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    let key = |c| KeyEvent {
        code: c,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    };
    let _ = app.take_dirty();
    app.on_key(key(KeyCode::Char('a')));
    assert!(app.dirty, "opening the add dialog must mark dirty");
    let _ = app.take_dirty();
    // Esc cancels and closes the dialog.
    app.on_key(key(KeyCode::Esc));
    assert!(app.dirty, "closing a dialog must mark dirty");
    let _ = app.take_dirty();

    // push_log itself marks dirty — pinned here to guard the
    // hot path (one of the most frequent sources of frames).
    app.push_log(make_log_line("hello"));
    assert!(app.dirty, "push_log must mark dirty");
}

/// The log scroll handlers must keep `ListState.offset` in
/// the valid range `[0, log.len().saturating_sub(1)]`, no
/// matter what direction the user scrolls.
#[test]
fn scroll_offset_clamps_to_valid_range() {
    let mut app = TuiApp::new(
        make_store(),
        None,
        "0.0.0.0:8085".into(),
        "http://localhost/v1".into(),
    );
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

fn make_app_with_capabilities(
    capabilities: std::sync::Arc<crate::capabilities::CapabilityStore>,
) -> TuiApp {
    TuiApp::new_with_stores(
        make_store(),
        std::sync::Arc::new(SessionStatsStore::new()),
        capabilities,
        None,
        "0.0.0.0:8085".into(),
        "http://localhost/v1".into(),
    )
}

fn render_to_string(app: &mut TuiApp, w: u16, h: u16) -> String {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            let area = frame.area();
            app.render(frame, area);
        })
        .expect("draw failed");
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(w as usize)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn press(app: &mut TuiApp, ch: char) -> bool {
    app.on_key(KeyEvent {
        code: KeyCode::Char(ch),
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    })
}

/// With an empty store nothing is learned, so no parameter names are
/// shown and no orphan group renders.
#[test]
fn no_learned_params_render_when_store_is_empty() {
    let mut app = make_app_with_capabilities(std::sync::Arc::new(
        crate::capabilities::CapabilityStore::new(),
    ));
    let rendered = render_to_string(&mut app, 140, 40);
    assert!(
        !rendered.contains("temp"),
        "no parameter names with an empty store:\n{rendered}"
    );
    assert!(!rendered.contains("learned (no mapping row)"), "{rendered}");
}

/// A learned rejection appears in the LEARNED column of the mapping
/// row whose upstream model it belongs to.
#[test]
fn learned_params_show_in_the_table_for_a_mapped_model() {
    let caps = std::sync::Arc::new(crate::capabilities::CapabilityStore::new());
    caps.record_unsupported(
        "gpt-5.6-luna",
        crate::capabilities::RequestParam::Temperature,
    );
    caps.record_unsupported("gpt-5.6-luna", crate::capabilities::RequestParam::TopP);
    let mut app = make_app_with_capabilities(caps);

    let rendered = render_to_string(&mut app, 140, 40);
    // The store is keyed by upstream model, so the fact belongs on
    // the row(s) that resolve to gpt-5.6-luna.
    assert!(rendered.contains("gpt-5.6-luna"), "{rendered}");
    assert!(rendered.contains("temp+top_p"), "{rendered}");
}

/// A fact whose mapping has since been deleted would otherwise be
/// invisible; it is surfaced in its own trailing group.
#[test]
fn learned_facts_without_a_mapping_row_are_listed() {
    let caps = std::sync::Arc::new(crate::capabilities::CapabilityStore::new());
    caps.record_unsupported(
        "gpt-6-astra",
        crate::capabilities::RequestParam::Temperature,
    );
    let mut app = make_app_with_capabilities(caps);

    let rendered = render_to_string(&mut app, 140, 40);
    assert!(
        rendered.replace(' ', "").contains("learned(nomappingrow)"),
        "{rendered}"
    );
    assert!(rendered.contains("gpt-6-astra"), "{rendered}");
}

/// `c` clears the blacklist, which is the operator's way to force a
/// re-probe after an upstream change without restarting the proxy.
#[test]
fn clear_key_empties_the_learned_registry() {
    let caps = std::sync::Arc::new(crate::capabilities::CapabilityStore::new());
    caps.record_unsupported(
        "gpt-6-astra",
        crate::capabilities::RequestParam::Temperature,
    );
    let mut app = make_app_with_capabilities(caps.clone());
    assert!(render_to_string(&mut app, 120, 40).contains("gpt-6-astra"));

    assert!(!press(&mut app, 'c'), "clear must not quit the app");
    assert_eq!(caps.load().model_count(), 0);
    assert!(
        !render_to_string(&mut app, 120, 40).contains("gpt-6-astra"),
        "cleared facts must disappear from the dashboard"
    );
}
