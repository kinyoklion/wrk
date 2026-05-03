mod pane;
mod proc;
mod settings;
mod store;
mod ui;

use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use notify::{RecursiveMode, Watcher, recommended_watcher};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;

use crate::pane::Focus;
use crate::pane::terminal::PtyPane;
use crate::settings::Settings;
use crate::store::{Project, ProjectStore};
use crate::ui::ModalState;
use crate::ui::modal::{AddProjectModal, ConfirmDeleteModal};

#[derive(Parser)]
#[command(name = "wrk", version, about = "TUI manager for concurrent Claude Code sessions")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List configured projects
    Ls,
    /// Add a project pointing at the given directory
    Add {
        path: PathBuf,
        #[arg(long)]
        name: Option<String>,
    },
    /// Remove a project by name
    Rm { name: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Ls) => cmd_ls(),
        Some(Command::Add { path, name }) => cmd_add(&path, name),
        Some(Command::Rm { name }) => cmd_rm(&name),
        None => run_tui(),
    }
}

fn cmd_ls() -> Result<()> {
    let store = store::load()?;
    if store.projects.is_empty() {
        println!("(no projects — add one with `wrk add <path>`)");
        return Ok(());
    }
    let max_name = store
        .projects
        .iter()
        .map(|p| p.name.len())
        .max()
        .unwrap_or(0);
    for p in &store.projects {
        println!("  {:<width$}  {}", p.name, p.path.display(), width = max_name);
    }
    Ok(())
}

fn cmd_add(path: &Path, name: Option<String>) -> Result<()> {
    let abs = path
        .canonicalize()
        .with_context(|| format!("resolving path {}", path.display()))?;
    if !abs.is_dir() {
        return Err(anyhow!("{} is not a directory", abs.display()));
    }
    let resolved_name = match name {
        Some(n) => n,
        None => abs
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("could not derive name from {}", abs.display()))?
            .to_string(),
    };
    let mut store = store::load()?;
    store.add(Project {
        name: resolved_name.clone(),
        path: abs,
        tags: vec![],
    })?;
    store::save(&store)?;
    println!("added '{resolved_name}'");
    Ok(())
}

fn cmd_rm(name: &str) -> Result<()> {
    let mut store = store::load()?;
    store.remove(name)?;
    store::save(&store)?;
    println!("removed '{name}'");
    Ok(())
}

pub struct App {
    pub store: ProjectStore,
    pub settings: Settings,
    pub sidebar: ui::projects::ProjectSidebar,
    pub focus: Focus,
    pub claude: Option<PtyPane>,
    pub shell: Option<PtyPane>,
    pub active_index: Option<usize>,
    pub modal: Option<ModalState>,
    pub should_quit: bool,
    pub last_size: (u16, u16),
    pub error: Option<String>,
    pub last_click: Option<(Instant, usize)>,
}

impl App {
    fn new(store: ProjectStore, settings: Settings) -> Self {
        let mut sidebar = ui::projects::ProjectSidebar::default();
        sidebar.refresh(&store);
        Self {
            store,
            settings,
            sidebar,
            focus: Focus::Projects,
            claude: None,
            shell: None,
            active_index: None,
            modal: None,
            should_quit: false,
            last_size: (0, 0),
            error: None,
            last_click: None,
        }
    }

    pub fn active_project(&self) -> Option<&Project> {
        self.active_index.and_then(|i| self.store.projects.get(i))
    }

    fn open_selected(&mut self, claude_size: (u16, u16), shell_size: (u16, u16)) {
        let Some(idx) = self.sidebar.selected_store_index() else {
            return;
        };
        let project = self.store.projects[idx].clone();
        self.active_index = Some(idx);
        self.sidebar.active = Some(project.name.clone());
        self.error = None;

        match PtyPane::spawn(
            &self.settings.claude_command,
            &project.path,
            claude_size.1,
            claude_size.0,
        ) {
            Ok(p) => self.claude = Some(p),
            Err(e) => {
                self.claude = None;
                self.error = Some(format!("claude spawn failed: {e}"));
            }
        }

        let shell_cmd = self.settings.shell();
        match PtyPane::spawn(&shell_cmd, &project.path, shell_size.1, shell_size.0) {
            Ok(p) => self.shell = Some(p),
            Err(e) => {
                self.shell = None;
                let msg = format!("shell spawn failed: {e}");
                self.error = Some(match self.error.take() {
                    Some(prev) => format!("{prev}; {msg}"),
                    None => msg,
                });
            }
        }

        self.focus = Focus::Claude;
    }

    fn reload_store(&mut self) -> Result<()> {
        self.store = store::load()?;
        self.sidebar.refresh(&self.store);
        if let Some(idx) = self.active_index
            && self.store.projects.get(idx).is_none()
        {
            self.active_index = None;
            self.sidebar.active = None;
            self.claude = None;
            self.shell = None;
        }
        Ok(())
    }
}

fn run_tui() -> Result<()> {
    let store = store::load()?;
    let settings = settings::load().unwrap_or_else(|e| {
        eprintln!("warning: failed to load settings.toml: {e}; using defaults");
        Settings::default()
    });
    let mut app = App::new(store, settings);

    enable_raw_mode().context("enabling raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("entering alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("creating terminal")?;

    let watch_rx = spawn_watcher()?;

    let res = event_loop(&mut terminal, &mut app, watch_rx);

    disable_raw_mode().ok();
    let mut stdout = io::stdout();
    execute!(stdout, DisableMouseCapture, LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    res
}

fn spawn_watcher() -> Result<Receiver<()>> {
    let (tx, rx) = channel::<()>();
    let path = store::config_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("config path has no parent"))?
        .to_path_buf();
    std::thread::spawn(move || {
        let watcher_tx = tx.clone();
        let mut watcher = match recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res
                && matches!(
                    event.kind,
                    notify::EventKind::Modify(_)
                        | notify::EventKind::Create(_)
                        | notify::EventKind::Remove(_)
                )
            {
                let _ = watcher_tx.send(());
            }
        }) {
            Ok(w) => w,
            Err(_) => return,
        };
        if watcher.watch(&parent, RecursiveMode::NonRecursive).is_err() {
            return;
        }
        std::thread::park();
    });
    Ok(rx)
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    watch_rx: Receiver<()>,
) -> Result<()> {
    while !app.should_quit {
        terminal.draw(|frame| ui::draw(frame, app))?;

        // Resize embedded panes to match the current layout.
        let area = terminal.size().context("terminal size")?;
        let body_area = Rect {
            x: 0,
            y: 0,
            width: area.width,
            height: area.height.saturating_sub(1),
        };
        let (claude_inner, shell_inner) = pane_inner_sizes(body_area);
        if let Some(p) = app.claude.as_mut() {
            let _ = p.resize(claude_inner.height, claude_inner.width);
        }
        if let Some(p) = app.shell.as_mut() {
            let _ = p.resize(shell_inner.height, shell_inner.width);
        }
        app.last_size = (claude_inner.width, claude_inner.height);

        // Drain external file watcher events.
        let mut reload = false;
        while watch_rx.try_recv().is_ok() {
            reload = true;
        }
        if reload {
            let _ = app.reload_store();
        }

        if event::poll(Duration::from_millis(33))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => handle_key(app, key)?,
                Event::Mouse(m) => handle_mouse(app, m, area.into()),
                Event::Resize(_, _) => {
                    // Next loop iteration re-resizes.
                }
                _ => {}
            }
        }

        if let Some(p) = app.claude.as_mut()
            && p.child_finished()
        {
            app.claude = None;
        }
        if let Some(p) = app.shell.as_mut()
            && p.child_finished()
        {
            app.shell = None;
        }
    }
    Ok(())
}

fn pane_inner_sizes(body: Rect) -> (Rect, Rect) {
    let (_, claude, shell) = pane_outer_rects(body);
    (inset(claude), inset(shell))
}

fn pane_outer_rects(body: Rect) -> (Rect, Rect, Rect) {
    use ratatui::layout::{Constraint, Direction, Layout};
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(24),
            Constraint::Percentage(50),
            Constraint::Min(20),
        ])
        .split(body);
    (cols[0], cols[1], cols[2])
}

fn body_rect(area: Rect) -> Rect {
    Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height.saturating_sub(1),
    }
}

fn inset(r: Rect) -> Rect {
    Rect {
        x: r.x + 1,
        y: r.y + 1,
        width: r.width.saturating_sub(2),
        height: r.height.saturating_sub(2),
    }
}

fn handle_key(app: &mut App, key: KeyEvent) -> Result<()> {
    if app.modal.is_some() {
        return handle_modal_key(app, key);
    }

    // Global keys (work from any pane, including inside a PTY).
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(' ') {
        app.focus = Focus::Projects;
        return Ok(());
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        match key.code {
            KeyCode::Char('q') => {
                app.should_quit = true;
                return Ok(());
            }
            KeyCode::Char('1') => {
                app.focus = Focus::Projects;
                return Ok(());
            }
            KeyCode::Char('2') => {
                app.focus = Focus::Claude;
                return Ok(());
            }
            KeyCode::Char('3') => {
                app.focus = Focus::Shell;
                return Ok(());
            }
            _ => {}
        }
    }

    match app.focus {
        Focus::Projects => handle_projects_key(app, key),
        Focus::Claude => {
            if let Some(pane) = app.claude.as_mut() {
                let bytes = pane::key_to_bytes(key);
                if !bytes.is_empty() {
                    let _ = pane.write(&bytes);
                }
            }
            Ok(())
        }
        Focus::Shell => {
            if let Some(pane) = app.shell.as_mut() {
                let bytes = pane::key_to_bytes(key);
                if !bytes.is_empty() {
                    let _ = pane.write(&bytes);
                }
            }
            Ok(())
        }
    }
}

fn handle_projects_key(app: &mut App, key: KeyEvent) -> Result<()> {
    if app.sidebar.filter.is_some() {
        return handle_filter_key(app, key);
    }
    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('j') | KeyCode::Down => app.sidebar.select_next(),
        KeyCode::Char('k') | KeyCode::Up => app.sidebar.select_prev(),
        KeyCode::Enter => {
            let area = current_pane_sizes(app);
            app.open_selected(area.0, area.1);
        }
        KeyCode::Char('+') => {
            app.modal = Some(ModalState::Add(AddProjectModal::default()));
        }
        KeyCode::Char('d') => {
            if let Some(idx) = app.sidebar.selected_store_index()
                && let Some(p) = app.store.projects.get(idx)
            {
                app.modal = Some(ModalState::ConfirmDelete(ConfirmDeleteModal {
                    project_name: p.name.clone(),
                }));
            }
        }
        KeyCode::Char('/') => {
            app.sidebar.filter = Some(String::new());
            app.sidebar.refresh(&app.store);
        }
        KeyCode::Char('r') => {
            app.reload_store()?;
        }
        _ => {}
    }
    Ok(())
}

fn handle_filter_key(app: &mut App, key: KeyEvent) -> Result<()> {
    let filter = app.sidebar.filter.as_mut().expect("filter active");
    match key.code {
        KeyCode::Esc => {
            app.sidebar.filter = None;
            app.sidebar.refresh(&app.store);
        }
        KeyCode::Enter => {
            // Keep filter applied; selection already at top.
            let area = current_pane_sizes(app);
            app.open_selected(area.0, area.1);
            app.sidebar.filter = None;
            app.sidebar.refresh(&app.store);
        }
        KeyCode::Backspace => {
            filter.pop();
            app.sidebar.refresh(&app.store);
        }
        KeyCode::Char(c) => {
            filter.push(c);
            app.sidebar.refresh(&app.store);
        }
        KeyCode::Down => app.sidebar.select_next(),
        KeyCode::Up => app.sidebar.select_prev(),
        _ => {}
    }
    Ok(())
}

fn handle_modal_key(app: &mut App, key: KeyEvent) -> Result<()> {
    let mut consumed_modal = None;
    match app.modal.as_mut().unwrap() {
        ModalState::Add(m) => match key.code {
            KeyCode::Esc => {
                consumed_modal = Some(());
            }
            KeyCode::Tab => m.toggle_focus(),
            KeyCode::Backspace => {
                m.current_input_mut().pop();
            }
            KeyCode::Char(c) => {
                m.current_input_mut().push(c);
            }
            KeyCode::Enter => {
                let path = PathBuf::from(m.path_input.trim());
                let name_input = m.name_input.trim().to_string();
                let result = (|| -> Result<()> {
                    let abs = path.canonicalize()?;
                    if !abs.is_dir() {
                        return Err(anyhow!("{} is not a directory", abs.display()));
                    }
                    let name = if name_input.is_empty() {
                        abs.file_name()
                            .and_then(|s| s.to_str())
                            .ok_or_else(|| anyhow!("could not derive name"))?
                            .to_string()
                    } else {
                        name_input
                    };
                    let mut store = store::load()?;
                    store.add(Project {
                        name,
                        path: abs,
                        tags: vec![],
                    })?;
                    store::save(&store)?;
                    Ok(())
                })();
                match result {
                    Ok(()) => {
                        consumed_modal = Some(());
                        app.reload_store()?;
                    }
                    Err(e) => m.error = Some(e.to_string()),
                }
            }
            _ => {}
        },
        ModalState::ConfirmDelete(m) => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let name = m.project_name.clone();
                let mut store = store::load()?;
                store.remove(&name).ok();
                store::save(&store)?;
                consumed_modal = Some(());
                app.reload_store()?;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                consumed_modal = Some(());
            }
            _ => {}
        },
    }
    if consumed_modal.is_some() {
        app.modal = None;
    }
    Ok(())
}

fn current_pane_sizes(_app: &App) -> ((u16, u16), (u16, u16)) {
    // (cols, rows) defaults; the next draw cycle will resize to actual dims.
    ((80, 24), (80, 24))
}

const DOUBLE_CLICK_MS: u128 = 350;

fn handle_mouse(app: &mut App, m: MouseEvent, area: Rect) {
    if app.modal.is_some() {
        return;
    }
    let MouseEventKind::Down(MouseButton::Left) = m.kind else {
        return;
    };

    let body = body_rect(area);
    let (sidebar_rect, claude_rect, shell_rect) = pane_outer_rects(body);
    let pos_x = m.column;
    let pos_y = m.row;

    if rect_contains(sidebar_rect, pos_x, pos_y) {
        handle_sidebar_click(app, sidebar_rect, pos_y);
        return;
    }
    if rect_contains(claude_rect, pos_x, pos_y) {
        app.focus = Focus::Claude;
        return;
    }
    if rect_contains(shell_rect, pos_x, pos_y) {
        app.focus = Focus::Shell;
    }
}

fn handle_sidebar_click(app: &mut App, sidebar: Rect, row: u16) {
    app.focus = Focus::Projects;
    let inner_top = sidebar.y + 1;
    let inner_bottom = sidebar.y + sidebar.height.saturating_sub(1);
    if row < inner_top || row >= inner_bottom {
        return;
    }
    let visible_idx =
        (row - inner_top) as usize + app.sidebar.state.offset();
    if visible_idx >= app.sidebar.filtered_indices.len() {
        return;
    }
    let now = Instant::now();
    let is_double = matches!(
        app.last_click,
        Some((t, idx))
            if idx == visible_idx
                && now.duration_since(t).as_millis() <= DOUBLE_CLICK_MS
    );
    app.sidebar.state.select(Some(visible_idx));
    app.last_click = Some((now, visible_idx));
    if is_double {
        let area = current_pane_sizes(app);
        app.open_selected(area.0, area.1);
        app.last_click = None;
    }
}

fn rect_contains(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}
