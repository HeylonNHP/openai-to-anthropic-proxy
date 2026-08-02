//! Rounded-panel rendering.
//!
//! All TUI panels share the same width and border glyphs so a
//! `<Enter>` edit dialog lines up with the main dashboard. Width
//! measurements use `unicode-width` so wide CJK characters in
//! status messages don't break the border alignment.

use unicode_width::UnicodeWidthStr;

/// Rounded-panel corner / side glyphs (single-line).
const TL: &str = "╭";
const TR: &str = "╮";
const BL: &str = "╰";
const BR: &str = "╯";
const H: &str = "─";
const V: &str = "│";
const ML: &str = "├";
const MR: &str = "┤";

/// A fully rendered panel: a list of rows, each of which is
/// already wrapped in left/right borders.
#[derive(Debug, Clone)]
pub struct Panel {
    /// Total width in terminal cells (including the two border cells).
    pub width: u16,
    /// Rendered lines, each exactly `width` cells.
    pub lines: Vec<String>,
}

impl Panel {
    /// `width` is the inner content width. Total panel width is
    /// `width + 2` (the two vertical borders).
    pub fn new(title: impl Into<String>, inner_width: u16) -> Self {
        let inner = inner_width as usize;
        let title = title.into();
        let total = inner + 2;

        // Top border: `╭─ TITLE ─────╮` (non-empty title) or
        // `╭─ ─────╮` (empty title). The visible content is `inner`
        // cells; we measure the title in display cells so wide CJK
        // glyphs don't blow the budget.
        let mut top = String::with_capacity(total);
        top.push_str(TL);
        top.push_str("─ ");
        let title_w = title.width();
        if title.is_empty() {
            // No title: fill the rest with dashes.
            for _ in 0..inner.saturating_sub(2) {
                top.push_str(H);
            }
        } else {
            top.push_str(&title);
            top.push(' ');
            let reserved = 2 + title_w + 1; // prefix + title + space
            let dash_count = inner.saturating_sub(reserved);
            for _ in 0..dash_count {
                top.push_str(H);
            }
        }
        top.push_str(TR);
        debug_assert_eq!(
            top.width(),
            total,
            "top border width mismatch: got {} expected {}",
            top.width(),
            total
        );

        // Bottom border: `╰───────────╯`
        let mut bot = String::with_capacity(total);
        bot.push_str(BL);
        for _ in 0..inner {
            bot.push_str(H);
        }
        bot.push_str(BR);
        debug_assert_eq!(bot.width(), total, "bottom border width mismatch");

        let mut lines = Vec::with_capacity(2);
        lines.push(top);
        lines.push(bot);
        Self {
            width: total as u16,
            lines,
        }
    }

    /// Append a horizontal rule (with optional left/right T-junction
    /// glyphs) of the panel's width.
    pub fn rule(&mut self) {
        let inner = self.width as usize - 2;
        let mut dashes = String::with_capacity(inner);
        for _ in 0..inner {
            dashes.push_str(H);
        }
        self.lines.push(format!("{ML}{dashes}{MR}"));
    }

    /// Append a content row. The text is truncated to the inner width
    /// and right-padded with spaces so the right border stays aligned
    /// regardless of the text's measured width.
    pub fn row(&mut self, text: &str) {
        let inner = self.width as usize - 2;
        let truncated = truncate_to_width(text, inner);
        let pad = inner.saturating_sub(truncated.width());
        let mut s = String::with_capacity(self.width as usize);
        s.push_str(V);
        s.push_str(&truncated);
        for _ in 0..pad {
            s.push(' ');
        }
        s.push_str(V);
        debug_assert_eq!(s.width(), self.width as usize, "row width mismatch");
        self.lines.push(s);
    }

    /// Number of rendered lines.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// True if no content rows have been added.
    pub fn is_empty(&self) -> bool {
        self.lines.len() <= 2
    }
}

/// Truncate `s` so its measured width does not exceed `max_width`.
/// If the string is already narrow enough, returns it unchanged.
pub(crate) fn truncate_to_width(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_owned();
    }
    let mut out = String::with_capacity(max_width);
    let mut w = 0usize;
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > max_width.saturating_sub(1) {
            // Leave room for an ellipsis on the right edge.
            out.push('…');
            w += 1;
            break;
        }
        out.push(c);
        w += cw;
    }
    // Pad with spaces if we cut early on a wide char boundary.
    while w < max_width {
        out.push(' ');
        w += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_and_bottom_align() {
        let p = Panel::new("test", 20);
        assert_eq!(p.width, 22);
        for line in &p.lines {
            assert_eq!(line.width(), p.width as usize);
        }
        assert!(p.lines[0].starts_with(TL));
        assert!(p.lines[0].ends_with(TR));
        assert!(p.lines[1].starts_with(BL));
        assert!(p.lines[1].ends_with(BR));
    }

    #[test]
    fn rows_pad_right_border() {
        let mut p = Panel::new("test", 10);
        p.row("hi");
        p.row("a long string that overflows the panel width");
        for line in &p.lines {
            assert_eq!(line.width(), p.width as usize, "misaligned: {line:?}");
            assert!(
                line.starts_with(V)
                    || line.starts_with(TL)
                    || line.starts_with(ML)
                    || line.starts_with(BL)
            );
            assert!(
                line.ends_with(V) || line.ends_with(TR) || line.ends_with(MR) || line.ends_with(BR)
            );
        }
    }

    #[test]
    fn rules_look_correct() {
        let mut p = Panel::new("test", 10);
        p.rule();
        assert_eq!(p.lines[2].chars().next(), Some('├'));
        assert_eq!(p.lines[2].chars().last(), Some('┤'));
    }

    #[test]
    fn empty_title_still_renders() {
        let p = Panel::new("", 4);
        assert_eq!(p.width, 6);
        for line in &p.lines {
            assert_eq!(line.width(), p.width as usize);
        }
    }
}
