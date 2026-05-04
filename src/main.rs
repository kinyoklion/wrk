mod pane;
mod proc;
mod settings;
mod store;
mod ui;

use std::collections::HashMap;
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

#[derive(Default)]
pub struct ProjectSession {
    pub claude: Option<PtyPane>,
    pub shell: Option<PtyPane>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    Split,
    Tabbed,
}

pub struct App {
    pub store: ProjectStore,
    pub settings: Settings,
    pub sidebar: ui::projects::ProjectSidebar,
    pub focus: Focus,
    pub sessions: HashMap<String, ProjectSession>,
    pub active_project_name: Option<String>,
    pub modal: Option<ModalState>,
    pub should_quit: bool,
    pub error: Option<String>,
    pub last_click: Option<(Instant, usize)>,
    pub layout_mode: LayoutMode,
    pub sidebar_hidden: bool,
    /// Width of the claude pane as a percentage of the content area in Split mode.
    pub claude_pct: u16,
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
            sessions: HashMap::new(),
            active_project_name: None,
            modal: None,
            should_quit: false,
            error: None,
            last_click: None,
            layout_mode: LayoutMode::Split,
            sidebar_hidden: false,
            claude_pct: 50,
        }
    }

    pub fn active_project(&self) -> Option<&Project> {
        let name = self.active_project_name.as_ref()?;
        self.store.projects.iter().find(|p| &p.name == name)
    }

    pub fn active_session(&self) -> Option<&ProjectSession> {
        let name = self.active_project_name.as_ref()?;
        self.sessions.get(name)
    }

    pub fn active_session_mut(&mut self) -> Option<&mut ProjectSession> {
        let name = self.active_project_name.as_ref()?.clone();
        self.sessions.get_mut(&name)
    }

    pub fn active_claude(&self) -> Option<&PtyPane> {
        self.active_session().and_then(|s| s.claude.as_ref())
    }

    pub fn active_shell(&self) -> Option<&PtyPane> {
        self.active_session().and_then(|s| s.shell.as_ref())
    }

    fn open_selected(&mut self, body: Rect) {
        let Some(idx) = self.sidebar.selected_store_index() else {
            return;
        };
        let project = self.store.projects[idx].clone();
        self.active_project_name = Some(project.name.clone());
        self.sidebar.active = Some(project.name.clone());
        self.error = None;

        let layout = compute_layout(body, self);
        let claude_inner = layout.claude_inner();
        let shell_inner = layout.shell_inner();

        let session = self
            .sessions
            .entry(project.name.clone())
            .or_default();

        // Spawn claude if missing or its child has died.
        let claude_dead = session
            .claude
            .as_mut()
            .is_some_and(|p| p.child_finished());
        if session.claude.is_none() || claude_dead {
            session.claude = match PtyPane::spawn(
                &self.settings.claude_command,
                &project.path,
                claude_inner.height,
                claude_inner.width,
            ) {
                Ok(p) => Some(p),
                Err(e) => {
                    push_error(&mut self.error, format!("claude spawn failed: {e}"));
                    None
                }
            };
        }

        // Spawn shell if missing or dead.
        let shell_dead = session
            .shell
            .as_mut()
            .is_some_and(|p| p.child_finished());
        if session.shell.is_none() || shell_dead {
            let cmd = self.settings.shell();
            session.shell = match PtyPane::spawn(
                &cmd,
                &project.path,
                shell_inner.height,
                shell_inner.width,
            ) {
                Ok(p) => Some(p),
                Err(e) => {
                    push_error(&mut self.error, format!("shell spawn failed: {e}"));
                    None
                }
            };
        }

        // Resize live panes to current geometry (covers terminal resize while inactive).
        if let Some(p) = session.claude.as_mut() {
            let _ = p.resize(claude_inner.height, claude_inner.width);
        }
        if let Some(p) = session.shell.as_mut() {
            let _ = p.resize(shell_inner.height, shell_inner.width);
        }

        self.focus = Focus::Claude;
    }

    fn reload_store(&mut self) -> Result<()> {
        self.store = store::load()?;
        self.sidebar.refresh(&self.store);

        // Drop sessions whose project is no longer in the store.
        let known: std::collections::HashSet<&str> =
            self.store.projects.iter().map(|p| p.name.as_str()).collect();
        self.sessions.retain(|name, _| known.contains(name.as_str()));

        if let Some(name) = &self.active_project_name
            && !known.contains(name.as_str())
        {
            self.active_project_name = None;
            self.sidebar.active = None;
        }
        Ok(())
    }
}

fn push_error(slot: &mut Option<String>, msg: String) {
    *slot = Some(match slot.take() {
        Some(prev) => format!("{prev}; {msg}"),
        None => msg,
    });
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

        let area: Rect = terminal.size().context("terminal size")?.into();
        let body = body_rect(area);
        let layout = compute_layout(body, app);
        let claude_inner = layout.claude_inner();
        let shell_inner = layout.shell_inner();

        // Resize only the active session's panes; inactive sessions keep their grids.
        if let Some(session) = app.active_session_mut() {
            if let Some(p) = session.claude.as_mut() {
                let _ = p.resize(claude_inner.height, claude_inner.width);
            }
            if let Some(p) = session.shell.as_mut() {
                let _ = p.resize(shell_inner.height, shell_inner.width);
            }
        }

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
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    handle_key(app, key, body)?
                }
                Event::Mouse(m) => handle_mouse(app, m, area),
                Event::Resize(_, _) => {
                    // Next loop iteration re-resizes.
                }
                _ => {}
            }
        }

        // Reap dead children on the active session so the placeholder shows.
        if let Some(session) = app.active_session_mut() {
            if let Some(p) = session.claude.as_mut()
                && p.child_finished()
            {
                session.claude = None;
            }
            if let Some(p) = session.shell.as_mut()
                && p.child_finished()
            {
                session.shell = None;
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct LayoutRects {
    pub sidebar: Option<Rect>,
    pub claude: Rect,
    pub shell: Rect,
    /// Tab strip rect (Tabbed mode only). Inside: left half = claude tab, right = shell.
    pub tab_strip: Option<Rect>,
}

impl LayoutRects {
    pub fn claude_inner(&self) -> Rect {
        inset(self.claude)
    }
    pub fn shell_inner(&self) -> Rect {
        inset(self.shell)
    }
}

pub fn compute_layout(body: Rect, app: &App) -> LayoutRects {
    use ratatui::layout::{Constraint, Direction, Layout};

    let (sidebar_rect, content) = if app.sidebar_hidden {
        (None, body)
    } else {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(24), Constraint::Min(20)])
            .split(body);
        (Some(cols[0]), cols[1])
    };

    match app.layout_mode {
        LayoutMode::Split => {
            let pct = app.claude_pct.clamp(10, 90);
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(pct), Constraint::Min(10)])
                .split(content);
            LayoutRects {
                sidebar: sidebar_rect,
                claude: cols[0],
                shell: cols[1],
                tab_strip: None,
            }
        }
        LayoutMode::Tabbed => {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(0)])
                .split(content);
            LayoutRects {
                sidebar: sidebar_rect,
                claude: rows[1],
                shell: rows[1],
                tab_strip: Some(rows[0]),
            }
        }
    }
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

fn handle_key(app: &mut App, key: KeyEvent, body: Rect) -> Result<()> {
    if app.modal.is_some() {
        return handle_modal_key(app, key, body);
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
            KeyCode::Char('0') => {
                app.sidebar_hidden = !app.sidebar_hidden;
                if app.sidebar_hidden && app.focus == Focus::Projects {
                    app.focus = Focus::Claude;
                }
                return Ok(());
            }
            KeyCode::Char('h') | KeyCode::Char(',') | KeyCode::Char('-') => {
                app.claude_pct = app.claude_pct.saturating_sub(5).max(10);
                return Ok(());
            }
            KeyCode::Char('l') | KeyCode::Char('.') | KeyCode::Char(']') => {
                app.claude_pct = (app.claude_pct + 5).min(90);
                return Ok(());
            }
            KeyCode::Char('t') => {
                app.layout_mode = match app.layout_mode {
                    LayoutMode::Split => LayoutMode::Tabbed,
                    LayoutMode::Tabbed => LayoutMode::Split,
                };
                if app.layout_mode == LayoutMode::Tabbed
                    && app.focus == Focus::Projects
                {
                    app.focus = Focus::Claude;
                }
                return Ok(());
            }
            _ => {}
        }
    }

    match app.focus {
        Focus::Projects => handle_projects_key(app, key, body),
        Focus::Claude => {
            if let Some(session) = app.active_session_mut()
                && let Some(pane) = session.claude.as_mut()
            {
                let bytes = pane::key_to_bytes(key, pane.app_cursor_mode());
                if !bytes.is_empty() {
                    let _ = pane.write(&bytes);
                }
            }
            Ok(())
        }
        Focus::Shell => {
            if let Some(session) = app.active_session_mut()
                && let Some(pane) = session.shell.as_mut()
            {
                let bytes = pane::key_to_bytes(key, pane.app_cursor_mode());
                if !bytes.is_empty() {
                    let _ = pane.write(&bytes);
                }
            }
            Ok(())
        }
    }
}

fn handle_projects_key(app: &mut App, key: KeyEvent, body: Rect) -> Result<()> {
    if app.sidebar.filter.is_some() {
        return handle_filter_key(app, key, body);
    }
    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('j') | KeyCode::Down => app.sidebar.select_next(),
        KeyCode::Char('k') | KeyCode::Up => app.sidebar.select_prev(),
        KeyCode::Enter => {
            app.open_selected(body);
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

fn handle_filter_key(app: &mut App, key: KeyEvent, body: Rect) -> Result<()> {
    let filter = app.sidebar.filter.as_mut().expect("filter active");
    match key.code {
        KeyCode::Esc => {
            app.sidebar.filter = None;
            app.sidebar.refresh(&app.store);
        }
        KeyCode::Enter => {
            app.open_selected(body);
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

fn handle_modal_key(app: &mut App, key: KeyEvent, _body: Rect) -> Result<()> {
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

const DOUBLE_CLICK_MS: u128 = 350;

fn handle_mouse(app: &mut App, m: MouseEvent, area: Rect) {
    if app.modal.is_some() {
        return;
    }
    let MouseEventKind::Down(MouseButton::Left) = m.kind else {
        return;
    };

    let body = body_rect(area);
    let layout = compute_layout(body, app);
    let pos_x = m.column;
    let pos_y = m.row;

    if let Some(sidebar_rect) = layout.sidebar
        && rect_contains(sidebar_rect, pos_x, pos_y)
    {
        handle_sidebar_click(app, sidebar_rect, pos_y, body);
        return;
    }

    // Tab strip click (only in Tabbed mode).
    if let Some(strip) = layout.tab_strip
        && rect_contains(strip, pos_x, pos_y)
    {
        let half = strip.width / 2;
        if pos_x < strip.x + half {
            app.focus = Focus::Claude;
        } else {
            app.focus = Focus::Shell;
        }
        return;
    }

    match app.layout_mode {
        LayoutMode::Split => {
            if rect_contains(layout.claude, pos_x, pos_y) {
                app.focus = Focus::Claude;
            } else if rect_contains(layout.shell, pos_x, pos_y) {
                app.focus = Focus::Shell;
            }
        }
        LayoutMode::Tabbed => {
            // claude == shell in tabbed mode; click in content keeps current focus
            // (or focuses whichever is currently visible). Just leave focus as-is
            // unless it's still on Projects.
            if rect_contains(layout.claude, pos_x, pos_y)
                && app.focus == Focus::Projects
            {
                app.focus = Focus::Claude;
            }
        }
    }
}

fn handle_sidebar_click(app: &mut App, sidebar: Rect, row: u16, body: Rect) {
    app.focus = Focus::Projects;
    let inner_top = sidebar.y + 1;
    let inner_bottom = sidebar.y + sidebar.height.saturating_sub(1);
    if row < inner_top || row >= inner_bottom {
        return;
    }
    let visible_idx = (row - inner_top) as usize + app.sidebar.state.offset();
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
        app.open_selected(body);
        app.last_click = None;
    }
}

fn rect_contains(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}
