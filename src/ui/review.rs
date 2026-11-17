//! Full-screen render for the code-review overlay: a file list on the left and
//! a side-by-side diff on the right, with a key-hint row along the bottom.
//! Navigation/state lives in `crate::review`; this module only draws.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::App;
use crate::review::diff::{DiffCell, FileStatus, RowKind};
use crate::review::{
    DiffRowHit, FileState, ReviewFocus, ReviewHitMap, ReviewSession, Side, VisualLine, display_text,
};
use crate::settings::Theme;
use crate::ui::{bordered, cell_width, truncate_to_width};

/// Width of the line-number gutter on each side (digits only; a space follows).
const NUM_W: usize = 4;
const SEP: &str = " │ ";

pub fn draw_review(frame: &mut Frame, app: &mut App, area: Rect) {
    frame.render_widget(Clear, area);
    let theme = app.theme;
    let Some(review) = app.review.as_mut() else {
        return;
    };
    // Re-recorded below by whatever this frame actually draws.
    review.hits = ReviewHitMap::default();

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

fn draw_file_list(frame: &mut Frame, area: Rect, review: &mut ReviewSession, theme: &Theme) {
    let focused = review.focus == ReviewFocus::Files;
    let block = bordered(theme)
        .title(" files ")
        .border_style(border(focused, theme));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    review.hits.files_pane = area;
    review.hits.files_inner = inner;

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
    // Scroll the list only as far as needed to keep the selection visible, so a
    // click on a row that's already in view doesn't shift the list under it.
    if review.selected < review.file_scroll {
        review.file_scroll = review.selected;
    } else if h > 0 && review.selected >= review.file_scroll + h {
        review.file_scroll = review.selected + 1 - h;
    }
    review.file_scroll = review
        .file_scroll
        .min(review.files.len().saturating_sub(h.max(1)));
    let top = review.file_scroll;
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
    let block = bordered(theme)
        .title(title)
        .border_style(border(focused, theme));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    review.hits.diff_pane = area;

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
    let (lines, mut row_hits, cur_item) = build_diff_lines(review, focused, text_w, w, theme);

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

    // Record what each drawn row shows, and where the before/after `│` sits:
    // past the marker column, the left cell (`cell_spans` is always gutter +
    // `text_w` wide), and the separator's leading space.
    row_hits.drain(..scroll.min(row_hits.len()));
    row_hits.truncate(h);
    review.hits.diff_body = body;
    review.hits.diff_rows = row_hits;
    let left_cell_w = NUM_W + 1 + text_w;
    review.hits.side_split = body.x.saturating_add((1 + left_cell_w + 1) as u16);
}

/// Build the full display list for the current file: one line per visual row/gap
/// plus a line for each attached comment. Returns the lines, a parallel list of
/// what each line shows (for mouse hit-testing), and the display index of the
/// cursor's row (so the caller can keep it scrolled into view).
fn build_diff_lines(
    review: &ReviewSession,
    focused: bool,
    text_w: usize,
    width: usize,
    theme: &Theme,
) -> (Vec<Line<'static>>, Vec<DiffRowHit>, usize) {
    let Some(fs) = review.current() else {
        return (vec![], vec![], 0);
    };
    let vlines = fs.visual_lines();
    if vlines.is_empty() {
        return (vec![], vec![], 0);
    }
    let cursor = fs.cursor.min(vlines.len() - 1);
    let active_side = focused.then_some(review.side);

    let mut lines = Vec::new();
    let mut hits = Vec::new();
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
        hits.push(DiffRowHit::Line(idx));
        // Interleave any comments attached to this row's before/after lines.
        if let VisualLine::Row(r) = vl {
            let row = &fs.file.rows[*r];
            for (side, cell) in [(Side::Before, &row.left), (Side::After, &row.right)] {
                if let Some(c) = cell.as_ref()
                    && let Some(body) = review.comment_for(review.selected, side, c.line)
                {
                    let comment = comment_lines(side, body, width, theme);
                    hits.extend(std::iter::repeat_n(
                        DiffRowHit::Comment { line: idx, side },
                        comment.len(),
                    ));
                    lines.extend(comment);
                }
            }
        }
    }
    (lines, hits, cur_item)
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
            let text = fit(&drop_cols(&display_text(&c.text), hscroll), text_w);
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
    // A wide char straddling the cut is dropped whole; pad the cell it leaves
    // so the rest of the line stays in the same columns as its neighbours.
    format!("{}{}", " ".repeat(used.saturating_sub(n)), chars.as_str())
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
    use crate::review::diff::{DiffTarget, ReviewFile, build_review_file};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// 61 lines with one replaced line in the middle (collapsed gaps either side).
    fn long_file() -> ReviewFile {
        let mut before = String::new();
        for i in 0..30 {
            before.push_str(&format!("context line {i}\n"));
        }
        before.push_str("old middle\n");
        for i in 0..30 {
            before.push_str(&format!("tail line {i}\n"));
        }
        let after = before.replace("old middle", "new middle");
        build_review_file("src/thing.rs".into(), FileStatus::Modified, &before, &after)
    }

    fn app_with(files: Vec<ReviewFile>) -> App {
        let mut app = App::new(Default::default(), Default::default());
        app.review = Some(ReviewSession::new(
            "proj".into(),
            Some("tab0".into()),
            DiffTarget::WorkingVsHead,
            files,
        ));
        app
    }

    fn app_with_review() -> App {
        app_with(vec![long_file()])
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
        // Cutting through a wide char pads its leftover cell so what follows
        // keeps its column.
        assert_eq!(drop_cols("漢字x", 1), " 字x");
        assert_eq!(drop_cols("漢字x", 2), "字x");
    }

    /// gofmt-style tab-indented code keeps its indentation and the before/after
    /// separator stays in one column on every row. (Tabs used to count as one
    /// cell when padding but draw as none, so each tab dragged the `│` a column
    /// left and the indentation vanished.)
    #[test]
    fn tab_indented_code_keeps_indentation_and_a_straight_separator() {
        let before = "func f(x int) int {\n\tif x > 0 {\n\t\treturn x\n\t}\n\treturn 0\n}\n";
        let after = before.replace("return 0", "return -1");
        let file = build_review_file("f.go".into(), FileStatus::Modified, before, &after);
        let mut app = app_with(vec![file]);
        app.review.as_mut().unwrap().set_all_revealed(true);
        let mut term = Terminal::new(TestBackend::new(100, 16)).unwrap();
        draw(&mut term, &mut app);

        let review = app.review.as_ref().unwrap();
        let (body, split) = (review.hits.diff_body, review.hits.side_split);
        let buf = term.backend().buffer();
        assert_eq!(review.hits.diff_rows.len(), 6, "every line is drawn");
        for i in 0..6 {
            let y = body.y + i;
            assert_eq!(
                buf[(split, y)].symbol(),
                "│",
                "separator out of line on row {i}:\n{}",
                buffer_text(&term)
            );
        }
        // Line 3 is two tabs deep: 8 spaces of indentation after the gutter.
        let (x, y) = find(&term, "return x");
        let gutter_end = body.x + 1 + NUM_W as u16 + 1;
        assert_eq!(x - gutter_end, 8, "indentation lost");
        assert_eq!(row_text(&term, y, gutter_end, x), " ".repeat(8));
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

    /// Buffer cell where `needle` first starts, scanning rows top to bottom.
    fn find(term: &Terminal<TestBackend>, needle: &str) -> (u16, u16) {
        let buf = term.backend().buffer();
        for y in 0..buf.area.height {
            let row: Vec<&str> = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            for x in 0..row.len() {
                if row[x..].concat().starts_with(needle) {
                    return (x as u16, y);
                }
            }
        }
        panic!("{needle:?} not on screen:\n{}", buffer_text(term));
    }

    /// A review of `n` small one-line-change files named `file00.rs`, ….
    fn app_with_files(n: usize) -> App {
        app_with(
            (0..n)
                .map(|i| {
                    build_review_file(
                        format!("file{i:02}.rs"),
                        FileStatus::Modified,
                        "a\nb\nc\nd\ne\nf\n",
                        "a\nb\nC\nd\ne\nf\n",
                    )
                })
                .collect(),
        )
    }

    /// The text of buffer row `y` within columns `[x0, x1)`.
    fn row_text(term: &Terminal<TestBackend>, y: u16, x0: u16, x1: u16) -> String {
        let buf = term.backend().buffer();
        (x0..x1).map(|x| buf[(x, y)].symbol()).collect()
    }

    fn draw(term: &mut Terminal<TestBackend>, app: &mut App) {
        term.draw(|f| draw_review(f, app, f.area())).unwrap();
    }

    /// Visual-line index of the first row of `kind` in the current file.
    fn first_row_of(review: &ReviewSession, kind: RowKind) -> usize {
        let fs = review.current().unwrap();
        fs.visual_lines()
            .iter()
            .position(|l| matches!(l, VisualLine::Row(r) if fs.file.rows[*r].kind == kind))
            .unwrap()
    }

    #[test]
    fn clicking_a_file_selects_and_focuses_it_then_the_diff_takes_focus_back() {
        let mut app = app_with_files(3);
        let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
        draw(&mut term, &mut app);
        assert_eq!(app.review.as_ref().unwrap().focus, ReviewFocus::Diff);

        let (x, y) = find(&term, "file02.rs");
        app.review.as_mut().unwrap().click(x, y);
        let review = app.review.as_ref().unwrap();
        assert_eq!(review.selected, 2);
        assert_eq!(review.focus, ReviewFocus::Files);

        // A click anywhere in the diff pane (here its border) refocuses it.
        draw(&mut term, &mut app);
        let pane = app.review.as_ref().unwrap().hits.diff_pane;
        app.review.as_mut().unwrap().click(pane.x, pane.y);
        let review = app.review.as_ref().unwrap();
        assert_eq!(review.focus, ReviewFocus::Diff);
        assert_eq!(review.selected, 2, "a diff click doesn't change the file");
    }

    #[test]
    fn clicking_a_diff_row_moves_the_cursor_and_picks_the_side_by_column() {
        let mut app = app_with_review();
        app.review.as_mut().unwrap().set_all_revealed(true);
        let target = first_row_of(app.review.as_ref().unwrap(), RowKind::Replace);
        let mut term = Terminal::new(TestBackend::new(100, 70)).unwrap();
        draw(&mut term, &mut app);

        let (x, y) = find(&term, "old middle");
        // The recorded split is exactly the drawn before/after separator.
        let split = app.review.as_ref().unwrap().hits.side_split;
        assert_eq!(term.backend().buffer()[(split, y)].symbol(), "│");
        app.review.as_mut().unwrap().click(x, y);
        let review = app.review.as_ref().unwrap();
        assert_eq!(review.current().unwrap().cursor, target);
        assert_eq!(review.side, Side::Before);

        draw(&mut term, &mut app);
        let (x, y) = find(&term, "new middle");
        app.review.as_mut().unwrap().click(x, y);
        let review = app.review.as_ref().unwrap();
        assert_eq!(review.current().unwrap().cursor, target);
        assert_eq!(review.side, Side::After);

        // Clicking the line number in the left gutter also counts as "before".
        draw(&mut term, &mut app);
        let (_, y) = find(&term, "context line 0");
        let body = app.review.as_ref().unwrap().hits.diff_body;
        app.review.as_mut().unwrap().click(body.x + 1, y);
        let review = app.review.as_ref().unwrap();
        assert_eq!(review.current().unwrap().cursor, 0);
        assert_eq!(review.side, Side::Before);
    }

    #[test]
    fn clicking_a_collapsed_gap_reveals_it() {
        let mut app = app_with_review();
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        draw(&mut term, &mut app);
        let collapsed = app
            .review
            .as_ref()
            .unwrap()
            .current()
            .unwrap()
            .visual_lines()
            .len();

        let (x, y) = find(&term, "unchanged line");
        app.review.as_mut().unwrap().click(x, y);
        let fs = app.review.as_ref().unwrap().current().unwrap();
        assert!(
            fs.visual_lines().len() > collapsed,
            "the gap should be revealed"
        );
        assert!(
            matches!(fs.visual_lines()[fs.cursor], VisualLine::Row(_)),
            "the cursor lands on the first revealed row, as with Enter"
        );
    }

    #[test]
    fn clicking_an_inline_comment_selects_its_row_and_side() {
        let mut app = app_with_review();
        let target;
        {
            let review = app.review.as_mut().unwrap();
            review.set_all_revealed(true);
            target = first_row_of(review, RowKind::Replace);
            review.current_mut().unwrap().cursor = target;
            review.set_side(Side::Before);
            review.begin_comment();
            for ch in "check this".chars() {
                review.editor_push_char(ch);
            }
            assert!(review.save_comment());
            // Move away and flip the side so the click has something to restore.
            review.current_mut().unwrap().cursor = 0;
            review.set_side(Side::After);
        }
        let mut term = Terminal::new(TestBackend::new(100, 70)).unwrap();
        draw(&mut term, &mut app);

        // The comment spans the full width; click its right half to show the
        // comment's own side wins over the column.
        let (_, y) = find(&term, "check this");
        let body = app.review.as_ref().unwrap().hits.diff_body;
        app.review
            .as_mut()
            .unwrap()
            .click(body.x + body.width - 2, y);
        let review = app.review.as_ref().unwrap();
        assert_eq!(review.current().unwrap().cursor, target);
        assert_eq!(review.side, Side::Before);
    }

    #[test]
    fn the_wheel_scrolls_whatever_the_pointer_is_over() {
        // A second file so the list has somewhere to go.
        let extra = build_review_file("z.rs".into(), FileStatus::Added, "", "new\n");
        let mut app = app_with(vec![long_file(), extra]);
        app.review.as_mut().unwrap().set_all_revealed(true);
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        draw(&mut term, &mut app);

        // Over the diff: the cursor moves; the file and focus don't change.
        let body = app.review.as_ref().unwrap().hits.diff_body;
        app.review.as_mut().unwrap().wheel(body.x + 5, body.y, true);
        let review = app.review.as_ref().unwrap();
        assert_eq!(review.current().unwrap().cursor, 3);
        assert_eq!((review.selected, review.focus), (0, ReviewFocus::Diff));

        // Over the file list: the selection steps, focus still stays put.
        let files = app.review.as_ref().unwrap().hits.files_inner;
        app.review.as_mut().unwrap().wheel(files.x, files.y, true);
        let review = app.review.as_ref().unwrap();
        assert_eq!((review.selected, review.focus), (1, ReviewFocus::Diff));
        app.review.as_mut().unwrap().wheel(files.x, files.y, false);
        assert_eq!(app.review.as_ref().unwrap().selected, 0);

        // Over neither (the hint row): the focused pane — the diff — scrolls.
        draw(&mut term, &mut app);
        app.review.as_mut().unwrap().wheel(50, 29, true);
        assert_eq!(app.review.as_ref().unwrap().current().unwrap().cursor, 6);
    }

    #[test]
    fn clicking_a_visible_file_does_not_shift_the_list() {
        let mut app = app_with_files(30);
        app.review.as_mut().unwrap().selected = 29;
        let mut term = Terminal::new(TestBackend::new(100, 12)).unwrap();
        draw(&mut term, &mut app);
        let scroll = app.review.as_ref().unwrap().file_scroll;
        assert!(scroll > 0, "the list is scrolled to show the last file");

        // Click the top visible row: it's selected, and the list stays put.
        let files = app.review.as_ref().unwrap().hits.files_inner;
        app.review.as_mut().unwrap().click(files.x + 2, files.y);
        assert_eq!(app.review.as_ref().unwrap().selected, scroll);
        draw(&mut term, &mut app);
        assert_eq!(app.review.as_ref().unwrap().file_scroll, scroll);
        let top_row = row_text(&term, files.y, files.x, files.x + files.width);
        assert!(
            top_row.contains(&format!("file{scroll:02}.rs")),
            "the clicked file is still on the top row: {top_row:?}"
        );
    }

    #[test]
    fn clicks_are_ignored_while_a_comment_is_being_typed() {
        let mut app = app_with_files(2);
        let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
        app.review.as_mut().unwrap().begin_comment();
        assert!(app.review.as_ref().unwrap().editing.is_some());
        draw(&mut term, &mut app);

        let (x, y) = find(&term, "file01.rs");
        app.review.as_mut().unwrap().click(x, y);
        app.review.as_mut().unwrap().wheel(x, y, true);
        let review = app.review.as_ref().unwrap();
        assert_eq!(review.selected, 0, "the draft's file stays selected");
        assert_eq!(review.focus, ReviewFocus::Diff);
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
