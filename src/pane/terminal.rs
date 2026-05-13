use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, TermMode, viewport_to_point};
use anyhow::{Context, Result};
use portable_pty::PtySize;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;
use vte::ansi::{Color as VteColor, NamedColor, Processor, StdSyncHandler};

use crate::proc::{self, Pty};

#[derive(Debug, Clone, Copy)]
struct TermSize {
    cols: usize,
    screen_lines: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }
    fn screen_lines(&self) -> usize {
        self.screen_lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

#[derive(Clone)]
struct PtyEventListener {
    writer: SharedWriter,
}

impl EventListener for PtyEventListener {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(s) = event
            && let Ok(mut w) = self.writer.lock()
        {
            let _ = w.write_all(s.as_bytes());
            let _ = w.flush();
        }
    }
}

pub struct PtyPane {
    term: Arc<Mutex<Term<PtyEventListener>>>,
    pty: Pty,
    writer: SharedWriter,
    reader_thread: Option<JoinHandle<()>>,
    /// Timestamp of the most recently received PTY output, updated by the
    /// reader thread. Used by the sidebar to infer "waiting for input"
    /// (no output for a while).
    last_output: Arc<Mutex<Instant>>,
    rows: u16,
    cols: u16,
}

impl PtyPane {
    pub fn spawn(
        command: &[String],
        cwd: &Path,
        rows: u16,
        cols: u16,
        env: &[(String, String)],
    ) -> Result<Self> {
        let pty = proc::spawn(command, cwd, rows, cols, env)?;
        Self::wrap(pty, rows, cols)
    }

    fn wrap(pty: Pty, rows: u16, cols: u16) -> Result<Self> {
        let size = TermSize {
            cols: cols as usize,
            screen_lines: rows as usize,
        };

        let raw_writer = pty.master.take_writer().context("taking pty writer")?;
        let writer: SharedWriter = Arc::new(Mutex::new(raw_writer));

        let listener = PtyEventListener {
            writer: Arc::clone(&writer),
        };
        let term = Term::new(Config::default(), &size, listener);
        let term = Arc::new(Mutex::new(term));

        let mut reader = pty
            .master
            .try_clone_reader()
            .context("cloning pty reader")?;

        let last_output = Arc::new(Mutex::new(Instant::now()));
        let last_output_for_reader = Arc::clone(&last_output);
        let term_for_reader = Arc::clone(&term);
        let reader_thread = thread::spawn(move || {
            let mut parser: Processor<StdSyncHandler> = Processor::new();
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut t) = last_output_for_reader.lock() {
                            *t = Instant::now();
                        }
                        if let Ok(mut t) = term_for_reader.lock() {
                            parser.advance(&mut *t, &buf[..n]);
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            term,
            pty,
            writer,
            reader_thread: Some(reader_thread),
            last_output,
            rows,
            cols,
        })
    }

    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let mut w = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("writer mutex poisoned"))?;
        w.write_all(bytes).context("writing to pty")?;
        w.flush().ok();
        Ok(())
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        if rows == self.rows && cols == self.cols {
            return Ok(());
        }
        self.rows = rows;
        self.cols = cols;
        let size = TermSize {
            cols: cols as usize,
            screen_lines: rows as usize,
        };
        if let Ok(mut term) = self.term.lock() {
            term.resize(size);
        }
        self.pty
            .master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("resizing pty")?;
        Ok(())
    }

    /// Returns the cursor position relative to the pane's render area, if visible.
    pub fn cursor(&self) -> Option<(u16, u16)> {
        let term = self.term.lock().ok()?;
        let content = term.renderable_content();
        if content.cursor.shape == alacritty_terminal::vte::ansi::CursorShape::Hidden {
            return None;
        }
        if content.display_offset > 0 {
            return None;
        }
        let p = content.cursor.point;
        Some((p.column.0 as u16, p.line.0 as u16))
    }

    pub fn child_finished(&mut self) -> bool {
        matches!(self.pty.child.try_wait(), Ok(Some(_)))
    }

    /// True when the embedded program has switched the terminal into
    /// "application cursor key" mode (DECCKM). When set, arrow keys and
    /// Home/End must be sent as `ESC O X` instead of `ESC [ X`.
    pub fn app_cursor_mode(&self) -> bool {
        self.term
            .lock()
            .map(|t| t.mode().contains(TermMode::APP_CURSOR))
            .unwrap_or(false)
    }

    /// True when the embedded program has enabled bracketed-paste mode
    /// (DECSET 2004). Callers wrap paste payloads in `\e[200~ … \e[201~`
    /// when set so the program can distinguish a paste from per-char input.
    pub fn bracketed_paste_mode(&self) -> bool {
        self.term
            .lock()
            .map(|t| t.mode().contains(TermMode::BRACKETED_PASTE))
            .unwrap_or(false)
    }

    /// Anchor a new selection at the viewport cell `(col, row)`, replacing
    /// any prior selection. `(col, row)` are 0-indexed and relative to the
    /// inner pane rect (the same shape `url_at` accepts).
    pub fn selection_anchor(&self, col: usize, row: usize) {
        let Ok(mut term) = self.term.lock() else {
            return;
        };
        if row >= term.screen_lines() || col >= term.columns() {
            return;
        }
        let display_offset = term.grid().display_offset();
        let point = viewport_to_point(display_offset, Point::new(row, Column(col)));
        term.selection = Some(Selection::new(SelectionType::Simple, point, Side::Left));
    }

    /// Extend the in-progress selection to viewport cell `(col, row)`. No-op
    /// if no selection is anchored. `col`/`row` are clamped to the grid so
    /// drag past the pane edges still updates to the nearest cell.
    pub fn selection_update(&self, col: usize, row: usize) {
        let Ok(mut term) = self.term.lock() else {
            return;
        };
        let cols = term.columns();
        let rows = term.screen_lines();
        if cols == 0 || rows == 0 {
            return;
        }
        let col = col.min(cols - 1);
        let row = row.min(rows - 1);
        let display_offset = term.grid().display_offset();
        let point = viewport_to_point(display_offset, Point::new(row, Column(col)));
        if let Some(sel) = term.selection.as_mut() {
            sel.update(point, Side::Right);
        }
    }

    /// Clear any in-progress selection on this pane.
    pub fn selection_clear(&self) {
        if let Ok(mut term) = self.term.lock() {
            term.selection = None;
        }
    }

    /// Materialize the current selection to a string, or `None` if there is
    /// no selection or it is empty. Mirrors alacritty's own copy semantics
    /// (handles wide chars, line wrapping, and scrollback correctly).
    pub fn selection_text(&self) -> Option<String> {
        let term = self.term.lock().ok()?;
        let s = term.selection_to_string()?;
        if s.is_empty() { None } else { Some(s) }
    }

    /// Snapshot of the terminal's current mouse-reporting flags.
    pub fn mouse_mode(&self) -> super::MouseMode {
        let Ok(term) = self.term.lock() else {
            return super::MouseMode::default();
        };
        let m = term.mode();
        super::MouseMode {
            report_click: m.contains(TermMode::MOUSE_REPORT_CLICK),
            drag: m.contains(TermMode::MOUSE_DRAG),
            motion: m.contains(TermMode::MOUSE_MOTION),
            sgr: m.contains(TermMode::SGR_MOUSE),
        }
    }

    /// Time elapsed since the most recent byte of PTY output.
    pub fn idle_for(&self) -> Duration {
        self.last_output
            .lock()
            .map(|t| t.elapsed())
            .unwrap_or_default()
    }

    /// Scroll the display by `delta` lines. Positive scrolls back into
    /// scrollback (older content), negative scrolls toward live output.
    pub fn scroll(&self, delta: i32) {
        if delta == 0 {
            return;
        }
        if let Ok(mut t) = self.term.lock() {
            t.scroll_display(Scroll::Delta(delta));
        }
    }

    pub fn scroll_to_bottom(&self) {
        if let Ok(mut t) = self.term.lock() {
            t.scroll_display(Scroll::Bottom);
        }
    }

    /// Returns the URL (OSC 8 or plain text) at the given viewport cell
    /// position, where (col, row) are 0-indexed within the terminal's visible
    /// area (i.e. relative to the inner pane rect, not the screen).
    pub fn url_at(&self, col: usize, row: usize) -> Option<String> {
        let term = self.term.lock().ok()?;
        let grid = term.grid();
        let display_offset = grid.display_offset();
        if row >= term.screen_lines() || col >= term.columns() {
            return None;
        }
        let gp = viewport_to_point(display_offset, Point::new(row, Column(col)));
        let cell = &grid[gp];

        if let Some(link) = cell.hyperlink() {
            let uri = link.uri().to_owned();
            if !uri.is_empty() {
                return Some(uri);
            }
        }

        // Fall back to scanning the text on this line for a plain URL.
        let cols = term.columns();
        let line_chars: Vec<char> = (0..cols)
            .map(|c| grid[viewport_to_point(display_offset, Point::new(row, Column(c)))].c)
            .collect();
        find_url_at(&line_chars, col)
    }

    /// Collect every URL visible in this pane's grid plus full scrollback,
    /// newest first, deduped by URL string. Cells with an OSC 8 hyperlink are
    /// recorded by their URI; plain-text URLs on each row are extracted with
    /// the same scheme/character rules as the click-based opener.
    pub fn collect_urls(&self) -> Vec<String> {
        let Ok(term) = self.term.lock() else {
            return Vec::new();
        };
        let grid = term.grid();
        let cols = term.columns();
        let top = grid.topmost_line().0;
        let bottom = grid.bottommost_line().0;

        let mut out: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let push = |s: String, out: &mut Vec<String>, seen: &mut HashSet<String>| -> bool {
            if s.is_empty() {
                return true;
            }
            if seen.insert(s.clone()) {
                out.push(s);
            }
            out.len() < URL_COLLECT_CAP
        };

        // Walk bottom-up so the most recent URLs come first.
        'outer: for line_i in (top..=bottom).rev() {
            let line = Line(line_i);
            // Track contiguous OSC 8 hyperlink runs so we record each one once.
            let mut prev_uri: Option<String> = None;
            let mut chars: Vec<char> = Vec::with_capacity(cols);
            for c in 0..cols {
                let cell = &grid[Point::new(line, Column(c))];
                chars.push(cell.c);
                let cur_uri = cell.hyperlink().map(|h| h.uri().to_owned());
                match (&prev_uri, &cur_uri) {
                    (Some(prev), Some(cur)) if prev == cur => {}
                    (_, Some(cur)) if !push(cur.clone(), &mut out, &mut seen) => {
                        break 'outer;
                    }
                    _ => {}
                }
                prev_uri = cur_uri;
            }
            for url in scan_line(&chars) {
                if !push(url, &mut out, &mut seen) {
                    break 'outer;
                }
            }
        }
        out
    }

    /// Write a textual snapshot of this pane's alacritty grid to `path`.
    /// Used to diagnose rendering bugs — captures dimensions, the active
    /// `TermMode` flags, the visible content per row (with control chars
    /// rendered as `?`), and a per-cell breakdown of every cell whose flags
    /// are non-default. The format is plain text, one line per logical entry.
    pub fn dump_grid(&self, path: &std::path::Path) -> Result<()> {
        let term = self
            .term
            .lock()
            .map_err(|_| anyhow::anyhow!("term mutex poisoned"))?;
        let grid = term.grid();
        let cols = term.columns();
        let lines = term.screen_lines();
        let display_offset = grid.display_offset();
        let mode = term.mode();

        let mut out = String::new();
        out.push_str(&format!(
            "wrk grid dump\nrows={lines} cols={cols} display_offset={display_offset}\nmode={mode:?}\n\n"
        ));

        // Visible content, row by row.
        out.push_str("--- visible content ---\n");
        for row in 0..lines {
            let mut line = String::with_capacity(cols);
            for col in 0..cols {
                let gp = viewport_to_point(display_offset, Point::new(row, Column(col)));
                let cell = &grid[gp];
                let c = if cell.c.is_control() { '?' } else { cell.c };
                line.push(c);
            }
            out.push_str(&format!("{row:3}: |{line}|\n"));
        }

        // Per-cell detail for cells with non-default flags or zero-width data.
        out.push_str("\n--- non-default cells ---\n");
        for row in 0..lines {
            for col in 0..cols {
                let gp = viewport_to_point(display_offset, Point::new(row, Column(col)));
                let cell = &grid[gp];
                let zw_count = cell.zerowidth().map(|z| z.len()).unwrap_or(0);
                if cell.flags.is_empty() && zw_count == 0 && !cell.c.is_control() {
                    continue;
                }
                out.push_str(&format!(
                    "{row:3},{col:3}: c={:?} flags={:?} zw={zw_count}\n",
                    cell.c, cell.flags
                ));
            }
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

impl Drop for PtyPane {
    fn drop(&mut self) {
        let _ = self.pty.child.kill();
        if let Some(h) = self.reader_thread.take() {
            let _ = h.join();
        }
    }
}

/// Renders the alacritty grid into a Ratatui buffer.
pub struct PtyPaneWidget<'a>(pub &'a PtyPane);

impl<'a> Widget for PtyPaneWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let Ok(term) = self.0.term.lock() else {
            return;
        };
        let content = term.renderable_content();
        let cols = term.columns();
        let lines = term.screen_lines();
        let display_offset = content.display_offset as i32;
        let selection = content.selection;

        for indexed in content.display_iter {
            let cell = indexed.cell;
            let line: i32 = indexed.point.line.0 + display_offset;
            let col = indexed.point.column.0 as i32;
            if line < 0 || col < 0 || line as usize >= lines || col as usize >= cols {
                continue;
            }
            let x = area.x + col as u16;
            let y = area.y + line as u16;
            if x >= area.x + area.width || y >= area.y + area.height {
                continue;
            }
            let selected = selection
                .as_ref()
                .is_some_and(|r| r.contains(indexed.point));
            let cell_buf = &mut buf[(x, y)];

            // The right half of a wide character (and the leading-spacer column-0
            // variant introduced by line wrap) is owned by the wide-char cell to
            // its left. Marking it `skip` keeps ratatui's diff from emitting it
            // as a separate update, which would otherwise leave a stale glyph in
            // the trailing cell whenever the wide char shifts to a different
            // column or disappears.
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                cell_buf.set_skip(true);
                continue;
            }
            cell_buf.set_skip(false);

            // Build the symbol: base char plus any zero-width combining marks
            // that alacritty stored on the cell. Replace control chars (which
            // shouldn't reach a cell, but defensively) with a space so we never
            // emit raw escape bytes through ratatui.
            let mut symbol = String::new();
            let base = if cell.c.is_control() { ' ' } else { cell.c };
            symbol.push(base);
            if let Some(zw) = cell.zerowidth() {
                for c in zw {
                    symbol.push(*c);
                }
            }
            cell_buf.set_symbol(&symbol);
            let mut style = cell_style(cell.fg, cell.bg, cell.flags);
            if selected {
                style = style.add_modifier(Modifier::REVERSED);
            }
            cell_buf.set_style(style);
        }
    }
}

fn cell_style(fg: VteColor, bg: VteColor, flags: Flags) -> Style {
    let mut style = Style::default()
        .fg(map_color(fg, true))
        .bg(map_color(bg, false));
    let mut mods = Modifier::empty();
    if flags.contains(Flags::BOLD) {
        mods |= Modifier::BOLD;
    }
    if flags.contains(Flags::ITALIC) {
        mods |= Modifier::ITALIC;
    }
    if flags.contains(Flags::UNDERLINE) {
        mods |= Modifier::UNDERLINED;
    }
    if flags.contains(Flags::INVERSE) {
        mods |= Modifier::REVERSED;
    }
    if flags.contains(Flags::DIM) {
        mods |= Modifier::DIM;
    }
    if flags.contains(Flags::HIDDEN) {
        mods |= Modifier::HIDDEN;
    }
    if flags.contains(Flags::STRIKEOUT) {
        mods |= Modifier::CROSSED_OUT;
    }
    style = style.add_modifier(mods);
    style
}

fn map_color(c: VteColor, is_fg: bool) -> Color {
    match c {
        VteColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        VteColor::Indexed(i) => Color::Indexed(i),
        VteColor::Named(name) => map_named(name, is_fg),
    }
}

fn map_named(n: NamedColor, is_fg: bool) -> Color {
    use NamedColor::*;
    match n {
        Black => Color::Black,
        Red => Color::Red,
        Green => Color::Green,
        Yellow => Color::Yellow,
        Blue => Color::Blue,
        Magenta => Color::Magenta,
        Cyan => Color::Cyan,
        White => Color::Gray,
        BrightBlack => Color::DarkGray,
        BrightRed => Color::LightRed,
        BrightGreen => Color::LightGreen,
        BrightYellow => Color::LightYellow,
        BrightBlue => Color::LightBlue,
        BrightMagenta => Color::LightMagenta,
        BrightCyan => Color::LightCyan,
        BrightWhite => Color::White,
        Foreground | DimForeground | BrightForeground => {
            let _ = is_fg;
            Color::Reset
        }
        Background | DimBlack => Color::Reset,
        DimRed => Color::Red,
        DimGreen => Color::Green,
        DimYellow => Color::Yellow,
        DimBlue => Color::Blue,
        DimMagenta => Color::Magenta,
        DimCyan => Color::Cyan,
        DimWhite => Color::Gray,
        Cursor => Color::Reset,
    }
}

const URL_COLLECT_CAP: usize = 500;

/// Find every plain-text URL on `chars`. Returns them in left-to-right order,
/// non-overlapping. Uses the same scheme list and character class as
/// [`find_url_at`].
fn scan_line(chars: &[char]) -> Vec<String> {
    let mut urls = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !is_url_char(chars[i]) {
            i += 1;
            continue;
        }
        let mut j = i;
        while j + 1 < chars.len() && is_url_char(chars[j + 1]) {
            j += 1;
        }
        let run: String = chars[i..=j].iter().collect();
        let trimmed = run.trim_end_matches(['.', ',', ')', ']', '>']);
        for scheme in &["https://", "http://", "ftp://"] {
            if let Some(pos) = trimmed.find(scheme) {
                urls.push(trimmed[pos..].to_string());
                break;
            }
        }
        i = j + 1;
    }
    urls
}

fn find_url_at(chars: &[char], col: usize) -> Option<String> {
    if col >= chars.len() || !is_url_char(chars[col]) {
        return None;
    }
    let mut start = col;
    while start > 0 && is_url_char(chars[start - 1]) {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < chars.len() && is_url_char(chars[end + 1]) {
        end += 1;
    }
    let candidate: String = chars[start..=end].iter().collect();
    // Trim trailing punctuation that is unlikely to be part of the URL.
    let candidate = candidate.trim_end_matches(['.', ',', ')', ']', '>']);
    for scheme in &["https://", "http://", "ftp://"] {
        if let Some(pos) = candidate.find(scheme) {
            return Some(candidate[pos..].to_string());
        }
    }
    None
}

fn is_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '-' | '.'
                | '_'
                | '~'
                | ':'
                | '/'
                | '?'
                | '#'
                | '['
                | ']'
                | '@'
                | '!'
                | '$'
                | '&'
                | '\''
                | '('
                | ')'
                | '*'
                | '+'
                | ','
                | ';'
                | '='
                | '%'
        )
}

/// Helper used by ui/mod.rs to position the terminal cursor on screen.
pub fn cursor_position(pane: &PtyPane, area: Rect) -> Option<Position> {
    let (col, row) = pane.cursor()?;
    Some(Position {
        x: area.x + col,
        y: area.y + row,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn scan_line_finds_multiple_urls() {
        let line = chars("see https://a.test/x and http://b.test, then ftp://c.test/path.");
        let urls = scan_line(&line);
        assert_eq!(
            urls,
            vec![
                "https://a.test/x".to_string(),
                "http://b.test".to_string(),
                "ftp://c.test/path".to_string(),
            ]
        );
    }

    #[test]
    fn scan_line_skips_non_url_text() {
        let line = chars("no url here, just words and (parens).");
        assert!(scan_line(&line).is_empty());
    }

    #[test]
    fn scan_line_trims_trailing_punctuation_and_brackets() {
        let line = chars("(see https://example.com).");
        assert_eq!(scan_line(&line), vec!["https://example.com".to_string()]);
    }
}
