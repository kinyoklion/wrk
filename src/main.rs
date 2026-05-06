mod pane;
mod proc;
mod session;
mod settings;
mod status;
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
use crate::store::{LayoutMode, Project, ProjectStore, SessionRef};
use crate::ui::ModalState;
use crate::ui::modal::{AddProjectModal, ClaudeTabPickerModal, ConfirmDeleteModal};

#[derive(Parser)]
#[command(
    name = "wrk",
    version,
    about = "TUI manager for concurrent Claude Code sessions"
)]
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
    /// Install Claude Code hooks into ~/.claude/settings.json so the
    /// sidebar can show precise per-project status.
    InstallHooks,
    /// Remove the wrk-installed hooks from ~/.claude/settings.json.
    UninstallHooks,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Ls) => cmd_ls(),
        Some(Command::Add { path, name }) => cmd_add(&path, name),
        Some(Command::Rm { name }) => cmd_rm(&name),
        Some(Command::InstallHooks) => cmd_install_hooks(),
        Some(Command::UninstallHooks) => cmd_uninstall_hooks(),
        None => run_tui(),
    }
}

fn cmd_install_hooks() -> Result<()> {
    let path = status::install_hooks()?;
    let dir = status::ensure_status_dir()?;
    println!("installed hooks in {}", path.display());
    println!("status files will live under {}", dir.display());
    println!("(no-op for any Claude session not launched by wrk)");
    Ok(())
}

fn cmd_uninstall_hooks() -> Result<()> {
    let (path, removed) = status::uninstall_hooks()?;
    println!("removed {removed} wrk hook entries from {}", path.display());
    Ok(())
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
        println!(
            "  {:<width$}  {}",
            p.name,
            p.path.display(),
            width = max_name
        );
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
        layout_mode: None,
        claude_sessions: vec![],
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

/// A single running (or dead) Claude tab within a project.
pub struct ClaudeTab {
    pub name: String,
    /// Claude session UUID — used to resume the same conversation.
    pub session_id: Option<String>,
    /// Unique ID for this tab's `WRK_STATUS_FILE` (stable for the lifetime of
    /// the tab, independent of the Claude session ID).
    pub status_id: String,
    pub pane: Option<PtyPane>,
    /// True when this tab was spawned as a brand-new session (`claude` with no
    /// args). We scan the filesystem after a short delay to capture the session
    /// ID so we can persist it for future restarts.
    pub detect_session_id: bool,
    /// Wall-clock time at which the pane was spawned, used to find the right
    /// session file when detecting the ID post-spawn.
    pub spawn_time: Option<std::time::SystemTime>,
}

static TAB_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn new_status_id() -> String {
    format!(
        "tab{}",
        TAB_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

#[derive(Default)]
pub struct ProjectSession {
    pub claude_tabs: Vec<ClaudeTab>,
    pub active_claude: usize,
    pub shell: Option<PtyPane>,
}

impl ProjectSession {
    pub fn active_claude_tab(&self) -> Option<&ClaudeTab> {
        self.claude_tabs.get(self.active_claude)
    }
    pub fn active_claude_tab_mut(&mut self) -> Option<&mut ClaudeTab> {
        self.claude_tabs.get_mut(self.active_claude)
    }
    pub fn active_claude_pane(&self) -> Option<&PtyPane> {
        self.active_claude_tab()?.pane.as_ref()
    }
    pub fn active_claude_pane_mut(&mut self) -> Option<&mut PtyPane> {
        self.active_claude_tab_mut()?.pane.as_mut()
    }
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
        self.active_session()?.active_claude_pane()
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
        self.layout_mode = project.layout_mode.unwrap_or_default();
        self.error = None;

        let layout = compute_layout(body, self);
        let claude_inner = layout.claude_inner();
        let shell_inner = layout.shell_inner();

        let session = self.sessions.entry(project.name.clone()).or_default();

        // Build the initial set of tabs from the project's configured sessions
        // (if none configured, fall back to one default --continue tab).
        if session.claude_tabs.is_empty() {
            if project.claude_sessions.is_empty() {
                session.claude_tabs.push(ClaudeTab {
                    name: "claude".to_string(),
                    session_id: None,
                    status_id: new_status_id(),
                    pane: None,
                    detect_session_id: false,
                    spawn_time: None,
                });
            } else {
                for sr in &project.claude_sessions {
                    session.claude_tabs.push(ClaudeTab {
                        name: sr.name.clone(),
                        session_id: sr.session_id.clone(),
                        status_id: new_status_id(),
                        pane: None,
                        detect_session_id: false,
                        spawn_time: None,
                    });
                }
            }
        }

        // Spawn any missing or dead claude panes.
        for tab in &mut session.claude_tabs {
            let dead = tab.pane.as_mut().is_some_and(|p| p.child_finished());
            if tab.pane.is_none() || dead {
                let cmd = claude_command(
                    &self.settings,
                    tab.session_id.as_deref(),
                    false,
                    &project.path,
                );
                let status_path = status::status_file_for_tab(&tab.status_id);
                let env = vec![(
                    "WRK_STATUS_FILE".to_string(),
                    status_path.to_string_lossy().into_owned(),
                )];
                let spawned = PtyPane::spawn(
                    &cmd,
                    &project.path,
                    claude_inner.height,
                    claude_inner.width,
                    &env,
                );
                match spawned {
                    Ok(p) => {
                        if tab.detect_session_id {
                            tab.spawn_time = Some(std::time::SystemTime::now());
                        }
                        tab.pane = Some(p);
                    }
                    Err(e) => push_error(&mut self.error, format!("claude spawn failed: {e}")),
                }
            }
        }

        // Spawn shell if missing or dead.
        let shell_dead = session.shell.as_mut().is_some_and(|p| p.child_finished());
        if session.shell.is_none() || shell_dead {
            let cmd = self.settings.shell();
            session.shell = match PtyPane::spawn(
                &cmd,
                &project.path,
                shell_inner.height,
                shell_inner.width,
                &[],
            ) {
                Ok(p) => Some(p),
                Err(e) => {
                    push_error(&mut self.error, format!("shell spawn failed: {e}"));
                    None
                }
            };
        }

        // Resize live panes to current geometry (covers terminal resize while inactive).
        for tab in &mut session.claude_tabs {
            if let Some(p) = tab.pane.as_mut() {
                let _ = p.resize(claude_inner.height, claude_inner.width);
            }
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
        let known: std::collections::HashSet<&str> = self
            .store
            .projects
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        self.sessions
            .retain(|name, _| known.contains(name.as_str()));

        if let Some(name) = &self.active_project_name
            && !known.contains(name.as_str())
        {
            self.active_project_name = None;
            self.sidebar.active = None;
        }

        // Re-sync layout from the active project (its layout may have been
        // edited externally).
        if let Some(p) = self.active_project() {
            self.layout_mode = p.layout_mode.unwrap_or_default();
        }
        Ok(())
    }

    /// Add a new Claude tab to the active project.
    /// `session_id = None` → fresh `claude` session (ID detected post-spawn).
    /// `session_id = Some(id)` → `claude --resume <id>`.
    fn add_claude_tab(&mut self, name: String, session_id: Option<String>, body: Rect) {
        let Some(project_name) = self.active_project_name.clone() else {
            return;
        };
        let layout = compute_layout(body, self);
        let claude_inner = layout.claude_inner();

        let Some(project_path) = self.active_project().map(|p| p.path.clone()) else {
            return;
        };

        let new_session = session_id.is_none();
        let cmd = claude_command(
            &self.settings,
            session_id.as_deref(),
            new_session,
            &project_path,
        );
        let status_id = new_status_id();
        let status_path = status::status_file_for_tab(&status_id);
        let env = vec![(
            "WRK_STATUS_FILE".to_string(),
            status_path.to_string_lossy().into_owned(),
        )];

        let spawn_result = PtyPane::spawn(
            &cmd,
            &project_path,
            claude_inner.height,
            claude_inner.width,
            &env,
        );
        let spawn_time = if new_session && spawn_result.is_ok() {
            Some(std::time::SystemTime::now())
        } else {
            None
        };
        let pane = match spawn_result {
            Ok(p) => Some(p),
            Err(e) => {
                push_error(&mut self.error, format!("claude spawn failed: {e}"));
                None
            }
        };

        let session = self.sessions.entry(project_name.clone()).or_default();
        session.claude_tabs.push(ClaudeTab {
            name,
            session_id: session_id.clone(),
            status_id,
            pane,
            detect_session_id: new_session,
            spawn_time,
        });
        let new_idx = session.claude_tabs.len() - 1;
        session.active_claude = new_idx;

        // Persist to store.
        self.persist_claude_sessions(&project_name);
    }

    /// Close (kill) the currently active Claude tab.  If it is the only tab,
    /// does nothing — a project always keeps at least one tab.
    fn close_active_claude_tab(&mut self) {
        let Some(project_name) = self.active_project_name.clone() else {
            return;
        };
        let Some(session) = self.sessions.get_mut(&project_name) else {
            return;
        };
        if session.claude_tabs.len() <= 1 {
            return;
        }
        session.claude_tabs.remove(session.active_claude);
        if session.active_claude >= session.claude_tabs.len() {
            session.active_claude = session.claude_tabs.len() - 1;
        }
        self.persist_claude_sessions(&project_name);
    }

    fn next_claude_tab(&mut self) {
        let Some(session) = self.active_session_mut() else {
            return;
        };
        if session.claude_tabs.len() > 1 {
            session.active_claude = (session.active_claude + 1) % session.claude_tabs.len();
        }
    }

    fn prev_claude_tab(&mut self) {
        let Some(session) = self.active_session_mut() else {
            return;
        };
        if session.claude_tabs.len() > 1 {
            let n = session.claude_tabs.len();
            session.active_claude = (session.active_claude + n - 1) % n;
        }
    }

    /// Write the current set of Claude tabs back to the project store and save.
    fn persist_claude_sessions(&mut self, project_name: &str) {
        let tabs = match self.sessions.get(project_name) {
            Some(s) => s
                .claude_tabs
                .iter()
                .map(|t| SessionRef {
                    name: t.name.clone(),
                    session_id: t.session_id.clone(),
                })
                .collect::<Vec<_>>(),
            None => return,
        };
        if let Some(p) = self
            .store
            .projects
            .iter_mut()
            .find(|p| p.name == project_name)
        {
            // Don't save the synthetic single default tab with no session_id
            // (that's the backwards-compat case — keep the TOML clean).
            let is_default_single =
                tabs.len() == 1 && tabs[0].name == "claude" && tabs[0].session_id.is_none();
            p.claude_sessions = if is_default_single { vec![] } else { tabs };
        }
        if let Err(e) = store::save(&self.store) {
            push_error(&mut self.error, format!("save failed: {e}"));
        }
    }

    /// Set the current layout mode and persist it on the active project.
    fn set_layout_mode(&mut self, mode: LayoutMode) {
        self.layout_mode = mode;
        let Some(name) = self.active_project_name.clone() else {
            return;
        };
        let mut changed = false;
        if let Some(p) = self.store.projects.iter_mut().find(|p| p.name == name) {
            if p.layout_mode != Some(mode) {
                p.layout_mode = Some(mode);
                changed = true;
            }
        }
        if changed && let Err(e) = store::save(&self.store) {
            push_error(&mut self.error, format!("save failed: {e}"));
        }
    }
}

/// Build the claude launch command for a tab.
///  - `session_id = Some(id)` → `claude --resume <id>`
///  - `new_session = true`    → `claude` (no args; start a fresh session)
///  - otherwise              → `claude --continue` only when prior sessions
///    exist on disk; bare `claude` for a new project
fn claude_command(
    settings: &Settings,
    session_id: Option<&str>,
    new_session: bool,
    project_path: &std::path::Path,
) -> Vec<String> {
    let mut cmd = settings.claude_base();
    match session_id {
        Some(id) => {
            cmd.push("--resume".to_string());
            cmd.push(id.to_string());
        }
        None if new_session => {
            // bare `claude` — Claude will create a fresh session
        }
        None => {
            // Use --continue only when a prior session actually exists; if the
            // project is brand-new there is nothing to continue and --continue
            // may exit immediately.
            if !session::discover_sessions(project_path).is_empty() {
                cmd.push("--continue".to_string());
            }
        }
    }
    cmd
}

/// For tabs spawned as new sessions, try to find the session ID that Claude
/// created on disk. We wait at least 3 s after spawn to give Claude time to
/// write its session file, then scan the project's session directory.
fn detect_new_session_ids(app: &mut App) {
    let Some(project_name) = app.active_project_name.clone() else {
        return;
    };
    let Some(project_path) = app.active_project().map(|p| p.path.clone()) else {
        return;
    };
    let Some(session) = app.sessions.get_mut(&project_name) else {
        return;
    };

    let mut any_detected = false;
    for tab in &mut session.claude_tabs {
        if !tab.detect_session_id || tab.session_id.is_some() {
            continue;
        }
        let Some(spawn_time) = tab.spawn_time else {
            continue;
        };
        let elapsed = spawn_time.elapsed().unwrap_or_default();
        if elapsed < Duration::from_secs(3) {
            continue;
        }
        if let Some(id) = session::find_session_created_after(&project_path, spawn_time) {
            tab.session_id = Some(id);
            tab.detect_session_id = false;
            tab.spawn_time = None;
            any_detected = true;
        } else if elapsed > Duration::from_secs(30) {
            // Give up waiting after 30 s.
            tab.detect_session_id = false;
            tab.spawn_time = None;
        }
    }

    if any_detected {
        let project_name = project_name.clone();
        app.persist_claude_sessions(&project_name);
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
    let _ = status::ensure_status_dir();
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
            for tab in &mut session.claude_tabs {
                if let Some(p) = tab.pane.as_mut() {
                    let _ = p.resize(claude_inner.height, claude_inner.width);
                }
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
                Event::Key(key) if key.kind == KeyEventKind::Press => handle_key(app, key, body)?,
                Event::Mouse(m) => handle_mouse(app, m, area),
                Event::Resize(_, _) => {
                    // Next loop iteration re-resizes.
                }
                _ => {}
            }
        }

        // Try to detect the session ID for newly-spawned "new session" tabs.
        detect_new_session_ids(app);

        // Reap dead children on the active session so the placeholder shows.
        if let Some(session) = app.active_session_mut() {
            for tab in &mut session.claude_tabs {
                if tab.pane.as_mut().is_some_and(|p| p.child_finished()) {
                    tab.pane = None;
                }
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
                let new_mode = match app.layout_mode {
                    LayoutMode::Split => LayoutMode::Tabbed,
                    LayoutMode::Tabbed => LayoutMode::Split,
                };
                app.set_layout_mode(new_mode);
                if app.layout_mode == LayoutMode::Tabbed && app.focus == Focus::Projects {
                    app.focus = Focus::Claude;
                }
                return Ok(());
            }
            // Claude tab management (works from any pane).
            KeyCode::Char('n') if app.active_project_name.is_some() => {
                let discovered = app
                    .active_project()
                    .map(|p| {
                        let known: std::collections::HashMap<String, String> = p
                            .claude_sessions
                            .iter()
                            .filter_map(|sr| {
                                sr.session_id
                                    .as_ref()
                                    .map(|id| (id.clone(), sr.name.clone()))
                            })
                            .collect();
                        session::discover_sessions_named(&p.path, &known)
                    })
                    .unwrap_or_default();
                app.modal = Some(ModalState::ClaudeTabPicker(ClaudeTabPickerModal::new(
                    &discovered,
                )));
                return Ok(());
            }
            KeyCode::Char('w') => {
                app.close_active_claude_tab();
                return Ok(());
            }
            KeyCode::Char('<') => {
                app.prev_claude_tab();
                return Ok(());
            }
            KeyCode::Char('>') => {
                app.next_claude_tab();
                return Ok(());
            }
            _ => {}
        }
    }

    match app.focus {
        Focus::Projects => handle_projects_key(app, key, body),
        Focus::Claude => {
            if let Some(session) = app.active_session_mut()
                && let Some(pane) = session.active_claude_pane_mut()
            {
                let bytes = pane::key_to_bytes(key, pane.app_cursor_mode());
                if !bytes.is_empty() {
                    let _ = pane.write(&bytes);
                    pane.scroll_to_bottom();
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
                    pane.scroll_to_bottom();
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

fn handle_modal_key(app: &mut App, key: KeyEvent, body: Rect) -> Result<()> {
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
                        layout_mode: None,
                        claude_sessions: vec![],
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
        ModalState::ClaudeTabPicker(m) => {
            if m.name_focused {
                match key.code {
                    KeyCode::Esc | KeyCode::Tab => m.name_focused = false,
                    KeyCode::Backspace => {
                        m.tab_name.pop();
                    }
                    KeyCode::Char(c) => m.tab_name.push(c),
                    KeyCode::Enter => {
                        m.confirmed = true;
                        consumed_modal = Some(());
                    }
                    _ => {}
                }
            } else {
                match key.code {
                    KeyCode::Esc => consumed_modal = Some(()),
                    KeyCode::Tab => m.name_focused = true,
                    KeyCode::Up => m.select_prev(),
                    KeyCode::Down => m.select_next(),
                    KeyCode::Enter => {
                        m.confirmed = true;
                        consumed_modal = Some(());
                    }
                    _ => {}
                }
            }
        }
    }
    if consumed_modal.is_some() {
        // Consume the ClaudeTabPicker result before clearing the modal.
        if let Some(ModalState::ClaudeTabPicker(m)) = app.modal.take() {
            if m.confirmed {
                let session_id = m.selected_session_id().map(|s| s.to_owned());
                let name = if m.tab_name.trim().is_empty() {
                    m.suggested_name()
                } else {
                    m.tab_name.trim().to_string()
                };
                app.add_claude_tab(name, session_id, body);
            }
        } else {
            app.modal = None;
        }
    }
    Ok(())
}

const DOUBLE_CLICK_MS: u128 = 350;

const SCROLL_LINES: i32 = 3;

fn handle_mouse(app: &mut App, m: MouseEvent, area: Rect) {
    if app.modal.is_some() {
        return;
    }

    let body = body_rect(area);
    let layout = compute_layout(body, app);
    let pos_x = m.column;
    let pos_y = m.row;

    // Sidebar clicks always belong to us, never to a PTY.
    if let Some(sidebar) = layout.sidebar
        && rect_contains(sidebar, pos_x, pos_y)
    {
        if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
            handle_sidebar_click(app, sidebar, pos_y, body);
        }
        return;
    }

    // Tab strip (Tabbed mode) — switch focus on click, ignore everything else.
    if let Some(strip) = layout.tab_strip
        && rect_contains(strip, pos_x, pos_y)
    {
        if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
            let half = strip.width / 2;
            if pos_x < strip.x + half {
                app.focus = Focus::Claude;
            } else {
                app.focus = Focus::Shell;
            }
        }
        return;
    }

    // Ctrl+left-click on a URL takes precedence over both PTY forwarding and
    // focus-switching.
    if let MouseEventKind::Down(MouseButton::Left) = m.kind
        && m.modifiers.contains(KeyModifiers::CONTROL)
        && try_open_url(app, &layout, pos_x, pos_y)
    {
        return;
    }

    // Forward to a PTY when its embedded program has enabled mouse reporting.
    if forward_mouse_to_pane(app, &layout, m, pos_x, pos_y) {
        return;
    }

    // Fallback: existing focus-on-click / scrollback behavior.
    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            handle_left_click(app, &layout, body, pos_x, pos_y);
        }
        MouseEventKind::ScrollUp => {
            scroll_at(app, &layout, pos_x, pos_y, SCROLL_LINES);
        }
        MouseEventKind::ScrollDown => {
            scroll_at(app, &layout, pos_x, pos_y, -SCROLL_LINES);
        }
        _ => {}
    }
}

/// Returns `true` if the event was forwarded to a PTY (or otherwise consumed
/// by the forwarding path) and the caller should not run fallback handling.
///
/// The forwarding rules:
/// - Click events forward only to the focused pane. Clicks on a different
///   pane fall through to focus-switching, then the next event (release/drag)
///   will be forwarded since focus matches by then.
/// - Drag/Move events forward only to the focused pane.
/// - Wheel events forward to whichever pane is under the cursor, regardless
///   of focus, so users can scroll without switching focus first — but only
///   when that pane has some mouse mode enabled. Otherwise we leave the
///   wheel events for the scrollback fallback.
fn forward_mouse_to_pane(
    app: &mut App,
    layout: &LayoutRects,
    m: MouseEvent,
    x: u16,
    y: u16,
) -> bool {
    // Determine which logical pane the cursor is over and the outer rect we
    // need to inset for coordinate conversion.
    let (target_focus, outer) = match app.layout_mode {
        LayoutMode::Split => {
            if rect_contains(layout.claude, x, y) {
                (Focus::Claude, layout.claude)
            } else if rect_contains(layout.shell, x, y) {
                (Focus::Shell, layout.shell)
            } else {
                return false;
            }
        }
        LayoutMode::Tabbed => {
            if !rect_contains(layout.claude, x, y) {
                return false;
            }
            // claude/shell rects share the same area; the visible pane is the
            // currently focused one, defaulting to claude if focus is on Projects.
            let f = match app.focus {
                Focus::Shell => Focus::Shell,
                _ => Focus::Claude,
            };
            (f, layout.claude)
        }
    };

    let inner = inset(outer);
    if x < inner.x || y < inner.y || inner.width == 0 || inner.height == 0 {
        return false;
    }
    let cx = x - inner.x + 1;
    let cy = y - inner.y + 1;
    if cx > inner.width || cy > inner.height {
        return false;
    }

    // Snapshot the pane's current mouse mode (drops the borrow before we
    // potentially re-borrow the session mutably below).
    let mode = {
        let Some(session) = app.active_session() else {
            return false;
        };
        let pane = match target_focus {
            Focus::Claude => session.active_claude_pane(),
            Focus::Shell => session.shell.as_ref(),
            Focus::Projects => None,
        };
        match pane {
            Some(p) => p.mouse_mode(),
            None => return false,
        }
    };

    let focus_match = app.focus == target_focus;
    let should_forward = match m.kind {
        MouseEventKind::Down(_) | MouseEventKind::Up(_) => mode.report_click && focus_match,
        MouseEventKind::Drag(_) => (mode.drag || mode.motion) && focus_match,
        MouseEventKind::Moved => mode.motion && focus_match,
        MouseEventKind::ScrollUp
        | MouseEventKind::ScrollDown
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => mode.any(),
    };
    if !should_forward {
        return false;
    }

    let bytes = pane::mouse_to_bytes(m, cx, cy, mode);
    if bytes.is_empty() {
        return false;
    }

    if let Some(session) = app.active_session_mut() {
        let pane = match target_focus {
            Focus::Claude => session.active_claude_pane_mut(),
            Focus::Shell => session.shell.as_mut(),
            Focus::Projects => None,
        };
        if let Some(p) = pane {
            let _ = p.write(&bytes);
            return true;
        }
    }
    false
}

fn handle_left_click(app: &mut App, layout: &LayoutRects, _body: Rect, pos_x: u16, pos_y: u16) {
    match app.layout_mode {
        LayoutMode::Split => {
            if rect_contains(layout.claude, pos_x, pos_y) {
                app.focus = Focus::Claude;
            } else if rect_contains(layout.shell, pos_x, pos_y) {
                app.focus = Focus::Shell;
            }
        }
        LayoutMode::Tabbed => {
            if rect_contains(layout.claude, pos_x, pos_y) && app.focus == Focus::Projects {
                app.focus = Focus::Claude;
            }
        }
    }
}

fn scroll_at(app: &App, layout: &LayoutRects, x: u16, y: u16, delta: i32) {
    let Some(session) = app.active_session() else {
        return;
    };
    let pane = match app.layout_mode {
        LayoutMode::Split => {
            if rect_contains(layout.claude, x, y) {
                session.active_claude_pane()
            } else if rect_contains(layout.shell, x, y) {
                session.shell.as_ref()
            } else {
                None
            }
        }
        LayoutMode::Tabbed => {
            if !rect_contains(layout.claude, x, y) {
                None
            } else {
                match app.focus {
                    Focus::Claude => session.active_claude_pane(),
                    Focus::Shell => session.shell.as_ref(),
                    _ => None,
                }
            }
        }
    };
    if let Some(p) = pane {
        p.scroll(delta);
    }
}

fn try_open_url(app: &App, layout: &LayoutRects, pos_x: u16, pos_y: u16) -> bool {
    let Some(session) = app.active_session() else {
        return false;
    };
    let (pane, outer) = match app.layout_mode {
        LayoutMode::Split => {
            if rect_contains(layout.claude, pos_x, pos_y) {
                (session.active_claude_pane(), layout.claude)
            } else if rect_contains(layout.shell, pos_x, pos_y) {
                (session.shell.as_ref(), layout.shell)
            } else {
                return false;
            }
        }
        LayoutMode::Tabbed => {
            if !rect_contains(layout.claude, pos_x, pos_y) {
                return false;
            }
            let pane = match app.focus {
                Focus::Claude => session.active_claude_pane(),
                Focus::Shell => session.shell.as_ref(),
                _ => return false,
            };
            (pane, layout.claude)
        }
    };
    let Some(pane) = pane else {
        return false;
    };
    let inner = inset(outer);
    if pos_x < inner.x || pos_y < inner.y {
        return false;
    }
    let col = (pos_x - inner.x) as usize;
    let row = (pos_y - inner.y) as usize;
    if let Some(url) = pane.url_at(col, row) {
        let _ = std::process::Command::new("xdg-open")
            .arg(&url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        return true;
    }
    false
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
