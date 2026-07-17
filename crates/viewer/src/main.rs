//! `wrk-md` — a standalone terminal markdown viewer.
//!
//! Renders a markdown file with the shared `wrk-markdown` engine, either as a
//! scrollable full-screen pager (default) or as plain text to stdout
//! (`--print`, for piping). Usable in any shell, independent of the `wrk` TUI.

use std::io::{self, Stdout, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
#[cfg(feature = "images")]
use crossterm::event::MouseButton;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
#[cfg(feature = "images")]
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders};
#[cfg(feature = "images")]
use ratatui::widgets::{Clear, Paragraph};
use wrk_markdown::{MarkdownView, MarkdownViewState, RenderOptions, RenderedDoc};

const SCROLL_LINES: i32 = 3;

#[derive(Parser)]
#[command(
    name = "wrk-md",
    version,
    about = "Standalone terminal markdown viewer"
)]
struct Cli {
    /// Markdown file to view.
    file: PathBuf,
    /// Disable syntax highlighting of code blocks.
    #[arg(long)]
    no_highlight: bool,
    /// Render to stdout as plain text instead of opening the pager.
    #[arg(long)]
    print: bool,
    /// Wrap width for `--print` (defaults to the terminal width, or 100 when
    /// piped). Has no effect in the interactive pager, which uses the pane width.
    #[arg(long)]
    width: Option<usize>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let source = std::fs::read_to_string(&cli.file)
        .with_context(|| format!("reading {}", cli.file.display()))?;

    let mut opts = RenderOptions::new(!cli.no_highlight);
    // Resolve relative image links against the file's own directory.
    if let Some(parent) = cli.file.parent().filter(|p| !p.as_os_str().is_empty()) {
        opts = opts.with_base_dir(parent);
    }

    if cli.print {
        // A plain-text dump would only flatten heading images back to text, so
        // skip generating them.
        opts.heading_images = false;
        // Explicit `--width`, else the terminal width, else a default for pipes.
        let width = cli.width.unwrap_or_else(|| {
            crossterm::terminal::size()
                .map(|(w, _)| w as usize)
                .unwrap_or(100)
        });
        let text = wrk_markdown::render_document(&source, width, &opts);
        let mut out = io::stdout().lock();
        out.write_all(wrk_markdown::to_plain_string(&text).as_bytes())?;
        return Ok(());
    }

    run_pager(&cli.file, opts)
}

fn run_pager(path: &std::path::Path, opts: RenderOptions) -> Result<()> {
    let source =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let title = format!(" {} ", path.display());

    enable_raw_mode().context("enabling raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture).context("entering alt screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("creating terminal")?;

    let res = pager_loop(&mut terminal, &title, path, opts, source);

    disable_raw_mode().ok();
    execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    res
}

fn pager_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    title: &str,
    path: &std::path::Path,
    mut opts: RenderOptions,
    mut source: String,
) -> Result<()> {
    let mut state = MarkdownViewState::new();
    // Detect the terminal's graphics protocol once, after the alternate screen
    // is up (per ratatui-image, the query must run here). `None` → no image
    // protocol; image blocks then fall back to their placeholder line. The same
    // query reports the terminal background, which auto-themes diagrams.
    #[cfg(feature = "images")]
    let picker = wrk_markdown::query_picker();
    #[cfg(feature = "images")]
    if let Some(p) = &picker {
        opts.diagram_ctx.prefers_dark = wrk_markdown::terminal_prefers_dark(p).unwrap_or(false);
        // Size heading images to the real terminal cell.
        let f = p.font_size();
        opts.cell_size = (f.width, f.height);
    }
    let mut doc = RenderedDoc::default();
    // Active fullscreen image viewer (zoom/pan), opened from an inline image.
    #[cfg(feature = "images")]
    let mut viewer: Option<wrk_markdown::ImageViewer> = None;
    // Re-render only when the content width changes (`0` forces the first render
    // and a re-render after reload).
    let mut rendered_width: u16 = 0;
    loop {
        // Content sits inside the 1-cell block border on each side.
        let inner_width = terminal.size()?.width.saturating_sub(2);
        if inner_width != rendered_width && inner_width > 0 {
            doc = wrk_markdown::render_blocks(&source, inner_width as usize, &opts);
            // Rasterize images for the new width (once per re-render, not frame).
            #[cfg(feature = "images")]
            if let Some(picker) = &picker {
                state.prepare_images(&doc, picker, inner_width);
            }
            rendered_width = inner_width;
        }

        terminal.draw(|frame| {
            let area = frame.area();
            // The fullscreen image viewer takes over the whole screen when open.
            #[cfg(feature = "images")]
            if let (Some(v), Some(p)) = (viewer.as_mut(), picker.as_ref()) {
                let rows = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(0), Constraint::Length(1)])
                    .split(area);
                frame.render_widget(Clear, rows[0]);
                v.render(rows[0], frame.buffer_mut(), p);
                let hint = format!(
                    " image · +/-/wheel zoom ({:.0}%) · hjkl/arrows pan · 0 reset · q/Esc close ",
                    v.zoom() * 100.0
                );
                frame.render_widget(
                    Paragraph::new(hint).style(Style::default().add_modifier(Modifier::DIM)),
                    rows[1],
                );
                return;
            }
            let block = Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().add_modifier(Modifier::DIM));
            let view = MarkdownView::new(&doc).block(block);
            frame.render_stateful_widget(view, area, &mut state);
        })?;

        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                // The image viewer captures all keys while open.
                #[cfg(feature = "images")]
                if viewer_key(&mut viewer, &key) {
                    // On close, force a re-render so the inline images rebuild
                    // their protocols and re-transmit after the fullscreen
                    // overlay clobbered their cells.
                    if viewer.is_none() {
                        rendered_width = 0;
                    }
                    continue;
                }
                if quit_requested(&key) {
                    return Ok(());
                }
                match key.code {
                    // Open the top image in view in the fullscreen zoom/pan viewer.
                    #[cfg(feature = "images")]
                    KeyCode::Enter => {
                        if let Some(idx) = state.top_visible_image()
                            && let Some(wrk_markdown::MdBlock::Image(img)) = doc.blocks.get(idx)
                        {
                            viewer = wrk_markdown::ImageViewer::open(&img.source);
                        }
                    }
                    KeyCode::Char('j') | KeyCode::Down => state.scroll_by(1),
                    KeyCode::Char('k') | KeyCode::Up => state.scroll_by(-1),
                    KeyCode::Char('d') | KeyCode::PageDown | KeyCode::Char(' ') => {
                        state.page_down()
                    }
                    KeyCode::Char('u') | KeyCode::PageUp => state.page_up(),
                    KeyCode::Char('g') | KeyCode::Home => state.scroll_to_top(),
                    KeyCode::Char('G') | KeyCode::End => state.scroll_to_bottom(),
                    KeyCode::Char('r') => {
                        if let Ok(fresh) = std::fs::read_to_string(path) {
                            source = fresh;
                            rendered_width = 0; // force re-render
                        }
                    }
                    // Toggle diagram background: transparent (blends with the
                    // terminal) ↔ opaque high-contrast card for hard-to-read
                    // diagrams. Force a re-render so it takes effect.
                    KeyCode::Char('b') => {
                        opts.diagram_ctx.opaque_background = !opts.diagram_ctx.opaque_background;
                        rendered_width = 0;
                    }
                    _ => {}
                }
            }
            Event::Mouse(m) => {
                // The image viewer captures the mouse (wheel zooms) while open.
                #[cfg(feature = "images")]
                if let Some(v) = viewer.as_mut() {
                    match m.kind {
                        MouseEventKind::ScrollUp => v.zoom_by(1.15),
                        MouseEventKind::ScrollDown => v.zoom_by(1.0 / 1.15),
                        _ => {}
                    }
                    continue;
                }
                match m.kind {
                    MouseEventKind::ScrollUp => state.scroll_by(-SCROLL_LINES),
                    MouseEventKind::ScrollDown => state.scroll_by(SCROLL_LINES),
                    // Left-click an inline image to open it in the viewer.
                    #[cfg(feature = "images")]
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(idx) = state.image_at(m.column, m.row)
                            && let Some(wrk_markdown::MdBlock::Image(img)) = doc.blocks.get(idx)
                        {
                            viewer = wrk_markdown::ImageViewer::open(&img.source);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// Handle a key while the fullscreen image viewer is open. Returns `true` when
/// the viewer is open (the key was consumed): zoom, pan, reset, or close.
#[cfg(feature = "images")]
fn viewer_key(viewer: &mut Option<wrk_markdown::ImageViewer>, key: &KeyEvent) -> bool {
    if viewer.is_none() {
        return false;
    }
    if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
        *viewer = None;
        return true;
    }
    if let Some(v) = viewer.as_mut() {
        match key.code {
            KeyCode::Char('+') | KeyCode::Char('=') => v.zoom_by(1.25),
            KeyCode::Char('-') | KeyCode::Char('_') => v.zoom_by(0.8),
            KeyCode::Char('0') => v.reset(),
            KeyCode::Char('h') | KeyCode::Left => v.pan_view(-0.2, 0.0),
            KeyCode::Char('l') | KeyCode::Right => v.pan_view(0.2, 0.0),
            KeyCode::Char('k') | KeyCode::Up => v.pan_view(0.0, -0.2),
            KeyCode::Char('j') | KeyCode::Down => v.pan_view(0.0, 0.2),
            _ => {}
        }
    }
    true
}

fn quit_requested(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}
