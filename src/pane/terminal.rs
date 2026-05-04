use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, TermMode};
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
    rows: u16,
    cols: u16,
}

impl PtyPane {
    pub fn spawn(command: &[String], cwd: &Path, rows: u16, cols: u16) -> Result<Self> {
        let pty = proc::spawn(command, cwd, rows, cols)?;
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

        let mut reader = pty.master.try_clone_reader().context("cloning pty reader")?;

        let term_for_reader = Arc::clone(&term);
        let reader_thread = thread::spawn(move || {
            let mut parser: Processor<StdSyncHandler> = Processor::new();
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
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
            let cell_buf = &mut buf[(x, y)];
            cell_buf.set_symbol(&cell.c.to_string());
            cell_buf.set_style(cell_style(cell.fg, cell.bg, cell.flags));
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

/// Helper used by ui/mod.rs to position the terminal cursor on screen.
pub fn cursor_position(pane: &PtyPane, area: Rect) -> Option<Position> {
    let (col, row) = pane.cursor()?;
    Some(Position {
        x: area.x + col,
        y: area.y + row,
    })
}
