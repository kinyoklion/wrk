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
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders};
use wrk_markdown::{MarkdownView, MarkdownViewState, RenderOptions};

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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let source = std::fs::read_to_string(&cli.file)
        .with_context(|| format!("reading {}", cli.file.display()))?;

    let opts = RenderOptions::new(!cli.no_highlight);

    if cli.print {
        let text = wrk_markdown::render_document(&source, &opts);
        let mut out = io::stdout().lock();
        out.write_all(wrk_markdown::to_plain_string(&text).as_bytes())?;
        return Ok(());
    }

    run_pager(&cli.file, opts)
}

fn run_pager(path: &std::path::Path, opts: RenderOptions) -> Result<()> {
    let mut source =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut text = wrk_markdown::render_document(&source, &opts);
    let title = format!(" {} ", path.display());

    enable_raw_mode().context("enabling raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture).context("entering alt screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("creating terminal")?;

    let res = pager_loop(&mut terminal, &mut text, &title, path, &opts, &mut source);

    disable_raw_mode().ok();
    execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    res
}

fn pager_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    text: &mut ratatui::text::Text<'static>,
    title: &str,
    path: &std::path::Path,
    opts: &RenderOptions,
    source: &mut String,
) -> Result<()> {
    let mut state = MarkdownViewState::new();
    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            let block = Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().add_modifier(Modifier::DIM));
            let view = MarkdownView::new(text).block(block);
            frame.render_stateful_widget(view, area, &mut state);
        })?;

        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if quit_requested(&key) {
                    return Ok(());
                }
                match key.code {
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
                            *source = fresh;
                            *text = wrk_markdown::render_document(source, opts);
                        }
                    }
                    _ => {}
                }
            }
            Event::Mouse(m) => match m.kind {
                MouseEventKind::ScrollUp => state.scroll_by(-SCROLL_LINES),
                MouseEventKind::ScrollDown => state.scroll_by(SCROLL_LINES),
                _ => {}
            },
            _ => {}
        }
    }
}

fn quit_requested(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}
