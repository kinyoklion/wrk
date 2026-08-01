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
use crate::review::{FileState, ReviewFocus, ReviewSession, VisualLine};
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

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    let (body, hint_area) = (rows[0], rows[1]);

    let files_w = (area.width / 4)
        .clamp(22, 40)
        .min(area.width.saturating_sub(20));
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(files_w), Constraint::Min(0)])
        .split(body);

    draw_file_list(frame, cols[0], review, &theme);
    draw_diff(frame, cols[1], review, &theme);
    frame.render_widget(Paragraph::new(hint_line(review, &theme)), hint_area);
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

    let Some(fs) = review.current_mut() else {
        centered(frame, inner, "No changes to review.", theme);
        return;
    };
    if fs.file.binary {
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
                format!("+{}", fs.file.added),
                Style::default().fg(theme.status_waiting),
            ),
            Span::raw("  "),
            Span::styled(
                format!("−{}", fs.file.removed),
                Style::default().fg(theme.status_attention),
            ),
        ])),
        head,
    );

    let lines = diff_body(fs, body, focused, theme);
    frame.render_widget(Paragraph::new(lines), body);
}

/// Build the visible slice of side-by-side diff lines, updating `fs.scroll` to
/// keep the cursor on screen.
fn diff_body(fs: &mut FileState, area: Rect, focused: bool, theme: &Theme) -> Vec<Line<'static>> {
    let vlines = fs.visual_lines();
    let h = area.height as usize;
    if vlines.is_empty() || h == 0 {
        return vec![];
    }
    let cursor = fs.cursor.min(vlines.len() - 1);
    if cursor < fs.scroll {
        fs.scroll = cursor;
    } else if cursor >= fs.scroll + h {
        fs.scroll = cursor + 1 - h;
    }
    fs.scroll = fs.scroll.min(vlines.len().saturating_sub(h));

    // Column geometry: marker | left(num+text) | " │ " | right(num+text).
    let w = area.width as usize;
    let side = w.saturating_sub(1 + cell_width(SEP)) / 2;
    let text_w = side.saturating_sub(NUM_W + 1);

    vlines
        .iter()
        .enumerate()
        .skip(fs.scroll)
        .take(h)
        .map(|(idx, vl)| render_line(vl, fs, idx == cursor && focused, text_w, theme))
        .collect()
}

fn render_line(
    vl: &VisualLine,
    fs: &FileState,
    is_cursor: bool,
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
            let mut spans = vec![marker];
            spans.extend(cell_spans(row.left.as_ref(), text_w, lc, theme));
            spans.push(Span::styled(
                SEP.to_string(),
                Style::default().fg(theme.border_unfocused),
            ));
            spans.extend(cell_spans(row.right.as_ref(), text_w, rc, theme));
            Line::from(spans)
        }
    }
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
    fg: Option<Color>,
    theme: &Theme,
) -> Vec<Span<'static>> {
    match cell {
        Some(c) => {
            let num = format!("{:>w$} ", c.line, w = NUM_W);
            let text = fit(&c.text, text_w);
            let text_style = match fg {
                Some(color) => Style::default().fg(color),
                None => Style::default(),
            };
            vec![
                Span::styled(num, Style::default().fg(theme.hint)),
                Span::styled(text, text_style),
            ]
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
    let ctx = format!(
        " review · {} · {} · ",
        review.project,
        review.target.label()
    );
    let keys = "↑↓ move · Tab files/diff · ⏎ reveal · e expand-all · o collapse-all · q close";
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
}
