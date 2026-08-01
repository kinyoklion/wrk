//! Full-screen render for the code-review overlay: a file list on the left and
//! a side-by-side diff on the right, with a key-hint row along the bottom.
//! Navigation/state lives in `crate::review`; this module only draws.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::App;
use crate::review::diff::{DiffCell, FileStatus, RowKind};
use crate::review::{FileState, ReviewFocus, ReviewSession, Side, VisualLine};
use crate::settings::Theme;
use crate::ui::{cell_width, truncate_to_width};

/// Width of the line-number gutter on each side (digits only; a space follows).
const NUM_W: usize = 4;
const SEP: &str = " │ ";

pub fn draw_review(frame: &mut Frame, app: &mut App, area: Rect) {
    frame.render_widget(Clear, area);
    let theme = app.theme;
    let Some(review) = app.review.as_mut() else {
        return;
    };

    // The comment editor sits above the hint while typing, growing (up to a cap)
    // to keep the wrapped text and caret visible.
    let editing = review.editing.is_some();
    let editor_h = review.editing.as_ref().map_or(0, |d| {
        let wrapped = wrap_text(&d.buffer, (area.width as usize).max(1)).len();
        // 1 label row + text rows, capped so the diff keeps most of the screen.
        (1 + wrapped.clamp(1, 4)) as u16
    });
    let constraints = if editing {
        vec![
            Constraint::Min(0),
            Constraint::Length(editor_h),
            Constraint::Length(1),
        ]
    } else {
        vec![Constraint::Min(0), Constraint::Length(1)]
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    let body = rows[0];
    let hint_area = rows[rows.len() - 1];

    let files_w = (area.width / 4)
        .clamp(22, 40)
        .min(area.width.saturating_sub(20));
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(files_w), Constraint::Min(0)])
        .split(body);

    draw_file_list(frame, cols[0], review, &theme);
    draw_diff(frame, cols[1], review, &theme);
    if editing {
        draw_comment_editor(frame, rows[1], review, &theme);
    }
    frame.render_widget(Paragraph::new(hint_line(review, &theme)), hint_area);
}

/// The comment input shown while typing (`c`): a labeled anchor row, then the
/// buffer wrapped across the remaining rows with a block cursor at the end. When
/// the text outgrows the box it scrolls so the caret (last line) stays visible.
fn draw_comment_editor(frame: &mut Frame, area: Rect, review: &ReviewSession, theme: &Theme) {
    let Some(d) = review.editing.as_ref() else {
        return;
    };
    let path = review
        .files
        .get(d.file)
        .map(|f| f.file.path.as_str())
        .unwrap_or("");
    let side = match d.side {
        Side::Before => "before",
        Side::After => "after",
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    let (label_area, text_area) = (rows[0], rows[1]);

    let label = format!(" comment {path}:{} ({side}) › ", d.line);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            fit(&label, label_area.width as usize),
            Style::default().bg(theme.accent).fg(theme.accent_fg),
        ))),
        label_area,
    );

    let w = (text_area.width as usize).max(1);
    let mut wrapped = wrap_text(&d.buffer, w);
    // Keep the caret (the last wrapped line) in view when the text overflows.
    let h = text_area.height as usize;
    let start = wrapped.len().saturating_sub(h.max(1));
    let last = wrapped.len() - 1;
    let lines: Vec<Line> = wrapped
        .drain(start..)
        .enumerate()
        .map(|(i, chunk)| {
            if start + i == last {
                Line::from(vec![
                    Span::raw(chunk),
                    Span::styled("▏", Style::default().fg(theme.accent)),
                ])
            } else {
                Line::from(Span::raw(chunk))
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), text_area);
}

fn border(focused: bool, theme: &Theme) -> Style {
    Style::default().fg(if focused {
        theme.border_focused
    } else {
        theme.border_unfocused
    })
}

fn status_color(status: &FileStatus, theme: &Theme) -> Color {
    match status {
        FileStatus::Added => theme.status_waiting,
        FileStatus::Modified => theme.status_busy,
        FileStatus::Deleted => theme.status_attention,
        FileStatus::Renamed { .. } => theme.accent,
    }
}

fn draw_file_list(frame: &mut Frame, area: Rect, review: &ReviewSession, theme: &Theme) {
    let focused = review.focus == ReviewFocus::Files;
    let block = Block::default()
        .title(" files ")
        .borders(Borders::ALL)
        .border_style(border(focused, theme));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if review.files.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no changes",
                Style::default().fg(theme.hint),
            ))),
            inner,
        );
        return;
    }

    let h = inner.height as usize;
    // Scroll the list to keep the selection visible.
    let top = review.selected.saturating_sub(h.saturating_sub(1));
    let lines: Vec<Line> = review
        .files
        .iter()
        .enumerate()
        .skip(top)
        .take(h)
        .map(|(i, fs)| file_line(fs, i == review.selected, focused, inner.width, theme))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn file_line(
    fs: &FileState,
    selected: bool,
    focused: bool,
    width: u16,
    theme: &Theme,
) -> Line<'static> {
    let glyph = fs.file.status.glyph();
    let stats = format!(" +{} −{}", fs.file.added, fs.file.removed);
    let avail = width as usize;
    // 2 = glyph + following space.
    let path_w = avail.saturating_sub(2 + cell_width(&stats)).max(1);
    let path = truncate_to_width(&fs.file.path, path_w);
    let pad = avail.saturating_sub(2 + cell_width(&path) + cell_width(&stats));

    if selected {
        let hl = Style::default()
            .bg(if focused {
                theme.accent
            } else {
                theme.border_unfocused
            })
            .fg(theme.accent_fg);
        Line::from(vec![Span::styled(
            format!("{glyph} {path}{}{stats}", " ".repeat(pad)),
            hl,
        )])
    } else {
        Line::from(vec![
            Span::styled(
                glyph.to_string(),
                Style::default()
                    .fg(status_color(&fs.file.status, theme))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" {path}{}", " ".repeat(pad))),
            Span::styled(stats, Style::default().fg(theme.hint)),
        ])
    }
}

fn draw_diff(frame: &mut Frame, area: Rect, review: &mut ReviewSession, theme: &Theme) {
    let focused = review.focus == ReviewFocus::Diff;
    let title = match review.current() {
        Some(fs) => format!(" {} ", fs.file.path),
        None => " review ".to_string(),
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border(focused, theme));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let (added, removed, binary) = match review.current() {
        Some(fs) => (fs.file.added, fs.file.removed, fs.file.binary),
        None => {
            centered(frame, inner, "No changes to review.", theme);
            return;
        }
    };
    if binary {
        centered(frame, inner, "Binary file — no textual diff.", theme);
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);
    let (head, body) = (rows[0], rows[1]);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("+{added}"),
                Style::default().fg(theme.status_waiting),
            ),
            Span::raw("  "),
            Span::styled(
                format!("−{removed}"),
                Style::default().fg(theme.status_attention),
            ),
        ])),
        head,
    );

    // Column geometry: marker | left(num+text) | " │ " | right(num+text).
    let w = body.width as usize;
    let side = w.saturating_sub(1 + cell_width(SEP)) / 2;
    let text_w = side.saturating_sub(NUM_W + 1);
    let (lines, cur_item) = build_diff_lines(review, focused, text_w, w, theme);

    // Scroll (in display-line space, which includes interleaved comments) to keep
    // the cursor's row visible.
    let h = body.height as usize;
    let scroll = match review.current_mut() {
        Some(fs) if h > 0 => {
            if cur_item < fs.scroll {
                fs.scroll = cur_item;
            } else if cur_item >= fs.scroll + h {
                fs.scroll = cur_item + 1 - h;
            }
            fs.scroll = fs.scroll.min(lines.len().saturating_sub(h));
            fs.scroll
        }
        _ => 0,
    };
    let visible: Vec<Line> = lines.into_iter().skip(scroll).take(h).collect();
    frame.render_widget(Paragraph::new(visible), body);
}

/// Build the full display list for the current file: one line per visual row/gap
/// plus a line for each attached comment. Returns the lines and the display
/// index of the cursor's row (so the caller can keep it scrolled into view).
fn build_diff_lines(
    review: &ReviewSession,
    focused: bool,
    text_w: usize,
    width: usize,
    theme: &Theme,
) -> (Vec<Line<'static>>, usize) {
    let Some(fs) = review.current() else {
        return (vec![], 0);
    };
    let vlines = fs.visual_lines();
    if vlines.is_empty() {
        return (vec![], 0);
    }
    let cursor = fs.cursor.min(vlines.len() - 1);
    let active_side = focused.then_some(review.side);

    let mut lines = Vec::new();
    let mut cur_item = 0;
    for (idx, vl) in vlines.iter().enumerate() {
        if idx == cursor {
            cur_item = lines.len();
        }
        lines.push(render_line(
            vl,
            fs,
            idx == cursor && focused,
            active_side,
            text_w,
            theme,
        ));
        // Interleave any comments attached to this row's before/after lines.
        if let VisualLine::Row(r) = vl {
            let row = &fs.file.rows[*r];
            if let Some(c) = row.left.as_ref()
                && let Some(body) = review.comment_for(review.selected, Side::Before, c.line)
            {
                lines.extend(comment_lines(Side::Before, body, width, theme));
            }
            if let Some(c) = row.right.as_ref()
                && let Some(body) = review.comment_for(review.selected, Side::After, c.line)
            {
                lines.extend(comment_lines(Side::After, body, width, theme));
            }
        }
    }
    (lines, cur_item)
}

fn render_line(
    vl: &VisualLine,
    fs: &FileState,
    is_cursor: bool,
    active_side: Option<Side>,
    text_w: usize,
    theme: &Theme,
) -> Line<'static> {
    let marker = Span::styled(
        if is_cursor { "▸" } else { " " }.to_string(),
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    );
    match vl {
        VisualLine::Gap { count, .. } => {
            let plural = if *count == 1 { "" } else { "s" };
            let text = format!("⋯ {count} unchanged line{plural} — ⏎ reveal ⋯");
            Line::from(vec![
                marker,
                Span::styled(
                    text,
                    Style::default()
                        .fg(theme.hint)
                        .add_modifier(Modifier::ITALIC),
                ),
            ])
        }
        VisualLine::Row(r) => {
            let row = &fs.file.rows[*r];
            let (lc, rc) = side_colors(row.kind, theme);
            let hscroll = fs.hscroll;
            let mut spans = vec![marker];
            spans.extend(cell_spans(
                row.left.as_ref(),
                text_w,
                hscroll,
                lc,
                active_side == Some(Side::Before),
                theme,
            ));
            spans.push(Span::styled(
                SEP.to_string(),
                Style::default().fg(theme.border_unfocused),
            ));
            spans.extend(cell_spans(
                row.right.as_ref(),
                text_w,
                hscroll,
                rc,
                active_side == Some(Side::After),
                theme,
            ));
            Line::from(spans)
        }
    }
}

/// Inline comment lines shown under the row they annotate, wrapped so a long
/// comment stays fully readable (the tag prefixes the first line; continuations
/// are indented to align under it).
fn comment_lines(side: Side, body: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let tag = match side {
        Side::Before => "before",
        Side::After => "after",
    };
    let prefix = format!("      💬 ({tag}) ");
    let indent = cell_width(&prefix);
    let avail = width.saturating_sub(indent).max(1);
    let style = Style::default().fg(theme.accent);
    wrap_text(body, avail)
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            let text = if i == 0 {
                format!("{prefix}{chunk}")
            } else {
                format!("{}{chunk}", " ".repeat(indent))
            };
            Line::from(Span::styled(text, style))
        })
        .collect()
}

/// (left fg, right fg) for a row kind; `None` cells render blank regardless.
fn side_colors(kind: RowKind, theme: &Theme) -> (Option<Color>, Option<Color>) {
    match kind {
        RowKind::Equal => (None, None),
        RowKind::Delete => (Some(theme.status_attention), None),
        RowKind::Insert => (None, Some(theme.status_waiting)),
        RowKind::Replace => (Some(theme.status_attention), Some(theme.status_waiting)),
    }
}

fn cell_spans(
    cell: Option<&DiffCell>,
    text_w: usize,
    hscroll: usize,
    fg: Option<Color>,
    active: bool,
    theme: &Theme,
) -> Vec<Span<'static>> {
    match cell {
        Some(c) => {
            let num = format!("{:>w$} ", c.line, w = NUM_W);
            let text = fit(&drop_cols(&c.text, hscroll), text_w);
            let text_style = match fg {
                Some(color) => Style::default().fg(color),
                None => Style::default(),
            };
            // The active comment side highlights its gutter so it's clear where
            // `c` will attach a comment.
            let num_style = if active {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.hint)
            };
            vec![Span::styled(num, num_style), Span::styled(text, text_style)]
        }
        None => vec![Span::raw(" ".repeat(NUM_W + 1 + text_w))],
    }
}

/// Truncate to `w` cells and right-pad with spaces so columns line up.
fn fit(s: &str, w: usize) -> String {
    let t = truncate_to_width(s, w);
    let pad = w.saturating_sub(cell_width(&t));
    format!("{t}{}", " ".repeat(pad))
}

/// Drop the first `n` display columns of `s` (for horizontal scroll).
fn drop_cols(s: &str, n: usize) -> String {
    if n == 0 {
        return s.to_string();
    }
    let mut used = 0;
    let mut chars = s.chars();
    for ch in chars.by_ref() {
        used += cell_width(ch.encode_utf8(&mut [0u8; 4]));
        if used >= n {
            break;
        }
    }
    chars.collect()
}

/// Greedy word-wrap to `width` cells, breaking at spaces where possible and hard-
/// splitting words longer than the width. Always returns at least one line.
fn wrap_text(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut line = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = cell_width(ch.encode_utf8(&mut [0u8; 4]));
        if w + cw > width {
            // Prefer to break at the last space on the line.
            if let Some(pos) = line.rfind(' ') {
                let rest = line.split_off(pos + 1);
                line.pop(); // drop the break space
                out.push(std::mem::replace(&mut line, rest));
            } else {
                out.push(std::mem::take(&mut line));
            }
            w = cell_width(&line);
        }
        line.push(ch);
        w += cw;
    }
    out.push(line);
    out
}

fn centered(frame: &mut Frame, area: Rect, msg: &str, theme: &Theme) {
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            msg.to_string(),
            Style::default().fg(theme.hint),
        )))
        .alignment(ratatui::layout::Alignment::Center),
        area,
    );
}

fn hint_line(review: &ReviewSession, theme: &Theme) -> Line<'static> {
    if review.editing.is_some() {
        return Line::from(Span::styled(
            " ⏎ save · Esc cancel · (empty ⏎ deletes) ".to_string(),
            Style::default().fg(theme.hint),
        ));
    }
    let n = review.comments.len();
    let ctx = format!(
        " review · {} · {} · {n} comment{} · ",
        review.project,
        review.target.label(),
        if n == 1 { "" } else { "s" },
    );
    let keys = "↑↓ move · <> hscroll · Tab files/diff · h/l side · c comment · D delete · \
                ⏎ reveal · e/o expand/collapse · q close";
    Line::from(vec![
        Span::styled(ctx, Style::default().fg(theme.accent)),
        Span::styled(keys.to_string(), Style::default().fg(theme.hint)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::diff::{DiffTarget, build_review_file};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app_with_review() -> App {
        let mut before = String::new();
        for i in 0..30 {
            before.push_str(&format!("context line {i}\n"));
        }
        before.push_str("old middle\n");
        for i in 0..30 {
            before.push_str(&format!("tail line {i}\n"));
        }
        let after = before.replace("old middle", "new middle");
        let file = build_review_file("src/thing.rs".into(), FileStatus::Modified, &before, &after);
        let mut app = App::new(Default::default(), Default::default());
        app.review = Some(ReviewSession::new(
            "proj".into(),
            Some("tab0".into()),
            DiffTarget::WorkingVsHead,
            vec![file],
        ));
        app
    }

    fn buffer_text(term: &Terminal<TestBackend>) -> String {
        term.backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    /// The overlay renders without panicking across sizes and shows the file and
    /// a collapsed-gap separator by default.
    #[test]
    fn overlay_renders_across_sizes() {
        for (w, h) in [(120u16, 40u16), (80, 24), (40, 12), (24, 8)] {
            let mut app = app_with_review();
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| draw_review(f, &mut app, f.area())).unwrap();
            let text = buffer_text(&term);
            if w >= 40 {
                assert!(text.contains("thing.rs"), "file name missing at {w}x{h}");
            }
        }
    }

    #[test]
    fn default_view_is_collapsed_and_expands() {
        let mut app = app_with_review();
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        term.draw(|f| draw_review(f, &mut app, f.area())).unwrap();
        assert!(
            buffer_text(&term).contains("unchanged line"),
            "expected a collapsed gap separator by default"
        );

        // Expand-all reveals every line → no separator remains.
        app.review.as_mut().unwrap().set_all_revealed(true);
        term.draw(|f| draw_review(f, &mut app, f.area())).unwrap();
        assert!(
            !buffer_text(&term).contains("unchanged line"),
            "expand-all should remove the collapsed separator"
        );
    }

    #[test]
    fn a_saved_comment_renders_inline() {
        let mut app = app_with_review();
        {
            let review = app.review.as_mut().unwrap();
            review.set_all_revealed(true);
            // Park the cursor on the changed row and leave a comment there.
            let target = review
                .current()
                .unwrap()
                .visual_lines()
                .iter()
                .position(|l| {
                    matches!(l, VisualLine::Row(r)
                        if review.current().unwrap().file.rows[*r].kind
                            == crate::review::diff::RowKind::Replace)
                })
                .unwrap();
            review.current_mut().unwrap().cursor = target;
            review.begin_comment();
            for ch in "needs a test".chars() {
                review.editor_push_char(ch);
            }
            assert!(review.save_comment());
        }
        let mut term = Terminal::new(TestBackend::new(100, 40)).unwrap();
        term.draw(|f| draw_review(f, &mut app, f.area())).unwrap();
        assert!(
            buffer_text(&term).contains("needs a test"),
            "the inline comment body should be visible"
        );
    }

    #[test]
    fn the_comment_editor_shows_its_anchor() {
        let mut app = app_with_review();
        {
            let review = app.review.as_mut().unwrap();
            review.set_all_revealed(true);
            review.current_mut().unwrap().cursor = 1;
            review.begin_comment();
        }
        let mut term = Terminal::new(TestBackend::new(100, 40)).unwrap();
        term.draw(|f| draw_review(f, &mut app, f.area())).unwrap();
        let text = buffer_text(&term);
        assert!(text.contains("comment"), "editor label missing: {text:?}");
        assert!(text.contains("save"), "editor hint missing");
    }

    #[test]
    fn wrap_text_breaks_at_spaces_and_hard_splits_long_words() {
        let lines = wrap_text("the quick brown fox", 9);
        assert!(lines.iter().all(|l| cell_width(l) <= 9), "{lines:?}");
        assert!(lines.len() >= 2);
        // A word longer than the width is hard-split rather than overflowing.
        let long = wrap_text("AAAAAAAAAAAAAAAAAAAA", 5);
        assert!(long.len() >= 4);
        assert!(long.iter().all(|l| cell_width(l) <= 5));
    }

    #[test]
    fn drop_cols_offsets_by_display_width() {
        assert_eq!(drop_cols("hello world", 6), "world");
        assert_eq!(drop_cols("hello", 0), "hello");
        assert_eq!(drop_cols("hi", 10), "");
    }

    #[test]
    fn horizontal_scroll_reveals_later_columns() {
        // A line long enough to overflow the diff pane, changed near the end.
        let tail = "x".repeat(60);
        let before = format!("keep\nlead {tail} END\n");
        let after = format!("keep\nlead {tail} DONE\n");
        let file = build_review_file("w.rs".into(), FileStatus::Modified, &before, &after);
        let mut app = App::new(Default::default(), Default::default());
        app.review = Some(ReviewSession::new(
            "p".into(),
            None,
            DiffTarget::WorkingVsHead,
            vec![file],
        ));
        let mut term = Terminal::new(TestBackend::new(90, 12)).unwrap();
        term.draw(|f| draw_review(f, &mut app, f.area())).unwrap();
        assert!(
            !buffer_text(&term).contains("DONE"),
            "the changed tail is off-screen before scrolling"
        );
        app.review.as_mut().unwrap().scroll_h(60);
        term.draw(|f| draw_review(f, &mut app, f.area())).unwrap();
        assert!(
            buffer_text(&term).contains("DONE"),
            "horizontal scroll should bring the tail into view"
        );
    }

    #[test]
    fn a_long_comment_wraps_instead_of_truncating() {
        let mut app = app_with_review();
        {
            let review = app.review.as_mut().unwrap();
            review.set_all_revealed(true);
            review.current_mut().unwrap().cursor = 1;
            review.begin_comment();
            for ch in "alpha bravo charlie delta echo foxtrot golf hotel india juliet".chars() {
                review.editor_push_char(ch);
            }
            assert!(review.save_comment());
        }
        let mut term = Terminal::new(TestBackend::new(70, 40)).unwrap();
        term.draw(|f| draw_review(f, &mut app, f.area())).unwrap();
        let text = buffer_text(&term);
        // The last word only appears if the comment wrapped rather than being cut.
        assert!(
            text.contains("juliet"),
            "long comment should wrap, not truncate"
        );
    }
}
