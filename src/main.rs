mod ipc;
mod keymap;
mod pane;
mod proc;
mod review;
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
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton,
    MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use notify::{RecursiveMode, Watcher, recommended_watcher};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;

use crate::keymap::{GlobalAction, KeyMap};
use crate::pane::Focus;
use crate::pane::terminal::PtyPane;
use crate::settings::{Settings, Theme};
use crate::store::{LayoutMode, Project, ProjectStore, SessionRef};
use crate::ui::ModalState;
use crate::ui::modal::{
    AddProjectModal, ClaudeTabPickerModal, ConfirmDeleteModal, ConfirmQuitModal,
    ConfirmUnloadModal, OpenMarkdownModal, UrlPickerModal,
};

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
    /// Open a markdown file in the wrk viewer. Inside a wrk pane it opens as a
    /// tab in the running instance; in a plain shell it opens a pager.
    View { path: PathBuf },
    /// Install Claude Code hooks into ~/.claude/settings.json (sidebar status)
    /// and a `wrk-view` skill into ~/.claude/skills so Claude can open files.
    InstallHooks,
    /// Remove the wrk-installed hooks and the `wrk-view` skill.
    UninstallHooks,
    /// Push a status update to the running wrk instance. Invoked by the Claude
    /// Code hooks installed by `install-hooks`; reads `WRK_SOCK`/`WRK_TAB` from
    /// the environment and is a silent no-op outside a wrk session.
    #[command(hide = true)]
    Hook { kind: String },
    /// Start or end an in-TUI code review in the running wrk instance:
    /// `wrk review start [target]` / `wrk review end`. `target` is a git rev or
    /// `a..b` range; empty compares the working tree to HEAD.
    Review {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Ls) => cmd_ls(),
        Some(Command::Add { path, name }) => cmd_add(&path, name),
        Some(Command::Rm { name }) => cmd_rm(&name),
        Some(Command::View { path }) => cmd_view(&path),
        Some(Command::InstallHooks) => cmd_install_hooks(),
        Some(Command::UninstallHooks) => cmd_uninstall_hooks(),
        Some(Command::Hook { kind }) => cmd_hook(&kind),
        Some(Command::Review { args }) => cmd_review(&args),
        None => run_tui(),
    }
}

/// Open a markdown file. When invoked from inside a wrk pane (`WRK_SOCK` set)
/// the request is sent to the running instance, which opens it as a tab in the
/// originating project (`WRK_PROJECT`). Otherwise — or if that socket is stale —
/// fall back to the standalone `wrk-md` pager.
fn cmd_view(path: &Path) -> Result<()> {
    let path = expand_tilde(&path.to_string_lossy());
    let abs = path
        .canonicalize()
        .with_context(|| format!("resolving {}", path.display()))?;

    if let Ok(sock) = std::env::var("WRK_SOCK") {
        let req = ipc::Request::Open(ipc::OpenRequest {
            path: abs.to_string_lossy().into_owned(),
            project: std::env::var("WRK_PROJECT").ok(),
        });
        if ipc::send(Path::new(&sock), &req).is_ok() {
            println!("opened {} in wrk", abs.display());
            return Ok(());
        }
        // Stale socket (instance exited) → fall through to the standalone viewer.
    }

    let prog = viewer_binary();
    let status = std::process::Command::new(&prog)
        .arg(&abs)
        .status()
        .with_context(|| format!("launching {}", prog.display()))?;
    std::process::exit(status.code().unwrap_or(1));
}

/// Locate the `wrk-md` viewer binary: prefer a sibling next to the current
/// executable (so it works from `target/release` without installing), else rely
/// on `PATH`.
fn viewer_binary() -> PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join("wrk-md");
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from("wrk-md")
}

fn cmd_install_hooks() -> Result<()> {
    let path = status::install_hooks()?;
    println!("installed hooks in {}", path.display());
    println!("hooks push status to the running wrk over its socket ($WRK_SOCK)");
    println!("(no-op for any Claude session not launched by wrk)");
    match status::install_skills() {
        Ok(skills) => {
            for path in skills {
                println!("installed skill {}", path.display());
            }
        }
        Err(e) => eprintln!("warning: could not install skills: {e}"),
    }
    Ok(())
}

/// Push a status update to the running wrk instance. Invoked by Claude Code
/// hooks as `wrk hook <kind>`; reads `WRK_SOCK` (the instance socket) and
/// `WRK_TAB` (the originating tab id) from the environment. Best-effort and
/// always exits 0 — a missing socket, stale instance, or unknown kind is a
/// silent no-op so hooks never surface errors to Claude.
fn cmd_hook(kind: &str) -> Result<()> {
    let (Ok(sock), Ok(tab)) = (std::env::var("WRK_SOCK"), std::env::var("WRK_TAB")) else {
        return Ok(());
    };
    let Some(kind) = status::StatusKind::from_arg(kind) else {
        return Ok(());
    };
    let req = ipc::Request::Status(ipc::StatusUpdate { tab, kind });
    let _ = ipc::send(Path::new(&sock), &req);
    Ok(())
}

/// Start or end an in-TUI code review in the running wrk instance:
/// `wrk review start [target]` opens the review overlay; `wrk review end`
/// closes it. Both require being inside a wrk-managed pane (`WRK_SOCK` set).
fn cmd_review(args: &[String]) -> Result<()> {
    let sub = args.first().map(String::as_str).unwrap_or("");
    let rest = args.get(1..).unwrap_or(&[]).join(" ");
    let sock = std::env::var("WRK_SOCK").ok().filter(|s| !s.is_empty());
    match sub {
        "start" => {
            let sock = sock.ok_or_else(|| {
                anyhow!("`wrk review` must be run inside a wrk-managed pane (no WRK_SOCK)")
            })?;
            let target = review::diff::DiffTarget::parse(&rest);
            let req = ipc::Request::Review(ipc::ReviewRequest {
                tab: std::env::var("WRK_TAB").ok(),
                project: std::env::var("WRK_PROJECT").ok(),
                kind: ipc::ReviewKind::Start {
                    target: if rest.trim().is_empty() {
                        None
                    } else {
                        Some(rest.trim().to_string())
                    },
                },
            });
            ipc::send(Path::new(&sock), &req).context("sending review request to wrk")?;
            println!(
                "Opened a code review ({}) in the wrk review pane. \
                 Review it there, then run /end-local-review to collect comments.",
                target.label()
            );
            Ok(())
        }
        "end" => {
            // Read the mirrored comments (written by the running TUI) and print
            // them as markdown for Claude to ingest, then ask the TUI to close.
            let key = review_env_key();
            let comments = review::read_mirror(&key);
            print!("{}", format_review_comments(&comments));
            if let Some(sock) = sock {
                let req = ipc::Request::Review(ipc::ReviewRequest {
                    tab: std::env::var("WRK_TAB").ok(),
                    project: std::env::var("WRK_PROJECT").ok(),
                    kind: ipc::ReviewKind::End,
                });
                let _ = ipc::send(Path::new(&sock), &req);
            }
            Ok(())
        }
        other => Err(anyhow!(
            "unknown `wrk review` subcommand '{other}'; use `start [target]` or `end`"
        )),
    }
}

/// Mirror-file key for an active review: the originating tab id, else the
/// project name.
fn review_mirror_key(session: &review::ReviewSession) -> String {
    session
        .tab_id
        .clone()
        .unwrap_or_else(|| session.project.clone())
}

/// The mirror key from the environment (client side of `wrk review end`):
/// `WRK_TAB`, falling back to `WRK_PROJECT`.
fn review_env_key() -> String {
    std::env::var("WRK_TAB")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("WRK_PROJECT").ok())
        .unwrap_or_default()
}

/// Render collected review comments as the markdown Claude reads on
/// `/end-local-review`.
fn format_review_comments(comments: &[review::ReviewComment]) -> String {
    if comments.is_empty() {
        return "No review comments were left.\n".to_string();
    }
    let mut out = format!("## Review comments ({})\n\n", comments.len());
    for c in comments {
        out.push_str(&format!(
            "- `{}:{}` ({}): {}\n  > {}\n",
            c.path,
            c.line,
            c.side,
            c.body.trim(),
            c.line_text.trim()
        ));
    }
    out
}

fn cmd_uninstall_hooks() -> Result<()> {
    let (path, removed) = status::uninstall_hooks()?;
    println!("removed {removed} wrk hook entries from {}", path.display());
    match status::uninstall_skills() {
        Ok(dirs) => {
            for dir in dirs {
                println!("removed skill {}", dir.display());
            }
        }
        Err(e) => eprintln!("warning: could not remove skills: {e}"),
    }
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

/// Expand a leading `~` / `~/…` to the user's home directory. The shell does
/// this for unquoted CLI args, but paths typed into wrk's modals — or quoted
/// arguments — reach us with the `~` literal, so `canonicalize` would fail.
/// Anything else (absolute or relative) is returned unchanged.
fn expand_tilde(input: &str) -> PathBuf {
    if input == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = input.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(input)
}

fn cmd_add(path: &Path, name: Option<String>) -> Result<()> {
    let path = expand_tilde(&path.to_string_lossy());
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
        shell_passthrough: None,
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
    /// Claude session UUID — used to resume the same conversation. `None` until
    /// the tab is first spawned; at that point wrk generates a fresh UUID,
    /// launches `claude --session-id <uuid>`, and records it here so every
    /// later open resumes deterministically via `--resume <uuid>`.
    pub session_id: Option<String>,
    /// Opaque per-tab id exported to the PTY as `WRK_TAB`, so hook-driven status
    /// pushes (`wrk hook`) can be routed back to this tab. Stable for the tab's
    /// lifetime, independent of the Claude session ID.
    pub status_id: String,
    /// Latest hook-driven status (state + sub-agent count). Defaults to "no
    /// event yet", at which point the UI falls back to the idle-time heuristic.
    pub status: status::TabStatus,
    pub pane: Option<PtyPane>,
}

static TAB_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn new_status_id() -> String {
    format!(
        "tab{}",
        TAB_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// A read-only markdown document tab within a project's primary pane. Unlike
/// Claude tabs these are ephemeral — they hold no PTY, aren't persisted to
/// `projects.toml`, and are dropped when the project is unloaded.
pub struct MarkdownTab {
    /// Display name shown in the tab strip (the file's name).
    pub name: String,
    /// Absolute path to the source file (used for reload).
    pub path: PathBuf,
    /// Raw markdown source. Re-rendered whenever the pane width changes so that
    /// tables lay out to the available width.
    pub source: String,
    /// Rendered block sequence (text + image refs), valid for `render_width`.
    pub rendered: wrk_markdown::RenderedDoc,
    /// Display width `rendered` was laid out at; `0` forces a (re-)render.
    pub render_width: u16,
    /// Color theme applied when rendering (resolved from settings at open).
    pub theme: wrk_markdown::MdTheme,
    /// Diagram rendering context: terminal dark/light (fixed at open) plus the
    /// per-tab opaque-background toggle (`b`).
    pub diagram_ctx: wrk_markdown::DiagramCtx,
    /// Whether H1–H3 render as true-size images (from settings, fixed at open).
    pub heading_images: bool,
    /// Terminal cell size in pixels, for sizing heading images (fixed at open).
    pub cell_size: (u16, u16),
    /// Scroll/viewport state for the markdown view widget.
    pub state: wrk_markdown::MarkdownViewState,
}

impl MarkdownTab {
    /// Re-render the document if the display width changed, returning `true`
    /// when it re-rendered (so the caller can rebuild image protocols). Cheap
    /// no-op when the width is unchanged (the common per-frame case).
    fn ensure_rendered(&mut self, width: u16) -> bool {
        if width == 0 || width == self.render_width {
            return false;
        }
        let mut opts = wrk_markdown::RenderOptions::default()
            .with_theme(self.theme)
            .with_diagram_ctx(self.diagram_ctx)
            .with_heading_images(self.heading_images)
            .with_cell_size(self.cell_size);
        // Resolve relative image links against the document's own directory.
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            opts = opts.with_base_dir(parent);
        }
        self.rendered = wrk_markdown::render_blocks(&self.source, width as usize, &opts);
        self.render_width = width;
        true
    }

    /// Toggle the diagram background between transparent (blends with the
    /// terminal) and opaque (a high-contrast card, for diagrams that are hard to
    /// read). Forces a re-render on the next draw so the change takes effect.
    fn toggle_diagram_background(&mut self) {
        self.diagram_ctx.opaque_background = !self.diagram_ctx.opaque_background;
        self.render_width = 0;
    }

    /// Re-read the file from disk; the next draw re-renders at the current width.
    fn reload(&mut self) {
        if let Ok(content) = std::fs::read_to_string(&self.path) {
            self.source = content;
            self.render_width = 0;
        }
    }
}

/// A tab in a project's primary pane: either a Claude PTY session or a markdown
/// document viewer.
pub enum Tab {
    Claude(ClaudeTab),
    Markdown(MarkdownTab),
}

impl Tab {
    pub fn name(&self) -> &str {
        match self {
            Tab::Claude(t) => &t.name,
            Tab::Markdown(t) => &t.name,
        }
    }
    pub fn as_claude(&self) -> Option<&ClaudeTab> {
        match self {
            Tab::Claude(t) => Some(t),
            Tab::Markdown(_) => None,
        }
    }
    pub fn as_claude_mut(&mut self) -> Option<&mut ClaudeTab> {
        match self {
            Tab::Claude(t) => Some(t),
            Tab::Markdown(_) => None,
        }
    }
    pub fn is_markdown(&self) -> bool {
        matches!(self, Tab::Markdown(_))
    }
}

#[derive(Default)]
pub struct ProjectSession {
    /// All tabs in the primary pane, Claude and markdown intermixed in the
    /// order they appear in the tab strip.
    pub tabs: Vec<Tab>,
    /// Index into `tabs` of the currently shown tab.
    pub active_tab: usize,
    pub shell: Option<PtyPane>,
}

impl ProjectSession {
    pub fn current(&self) -> Option<&Tab> {
        self.tabs.get(self.active_tab)
    }
    pub fn current_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active_tab)
    }
    pub fn active_claude_tab(&self) -> Option<&ClaudeTab> {
        self.current()?.as_claude()
    }
    pub fn active_claude_tab_mut(&mut self) -> Option<&mut ClaudeTab> {
        self.current_mut()?.as_claude_mut()
    }
    pub fn active_claude_pane(&self) -> Option<&PtyPane> {
        self.active_claude_tab()?.pane.as_ref()
    }
    pub fn active_claude_pane_mut(&mut self) -> Option<&mut PtyPane> {
        self.active_claude_tab_mut()?.pane.as_mut()
    }
    /// Iterator over just the Claude tabs (for spawning/resizing/reaping PTYs,
    /// status, and persistence — markdown tabs are skipped).
    pub fn claude_tabs(&self) -> impl Iterator<Item = &ClaudeTab> {
        self.tabs.iter().filter_map(Tab::as_claude)
    }
    pub fn claude_tabs_mut(&mut self) -> impl Iterator<Item = &mut ClaudeTab> {
        self.tabs.iter_mut().filter_map(Tab::as_claude_mut)
    }
}

pub struct App {
    pub store: ProjectStore,
    pub settings: Settings,
    /// Resolved chrome theme. Cached at startup from `settings.theme`.
    pub theme: Theme,
    /// Resolved markdown palette. Cached at startup from `settings.markdown`;
    /// applied to markdown tabs when they render.
    pub md_theme: wrk_markdown::MdTheme,
    /// Resolved global key bindings. Cached at startup from `settings.keys.global`.
    pub keymap: KeyMap,
    pub sidebar: ui::projects::ProjectSidebar,
    pub focus: Focus,
    pub sessions: HashMap<String, ProjectSession>,
    pub active_project_name: Option<String>,
    pub modal: Option<ModalState>,
    pub should_quit: bool,
    pub error: Option<String>,
    /// Transient informational status, rendered in the status bar without
    /// the "error:" prefix and auto-cleared on the next key or mouse event.
    /// Use for ephemeral feedback like "copied N chars".
    pub info: Option<String>,
    pub last_click: Option<(Instant, usize)>,
    pub layout_mode: LayoutMode,
    pub sidebar_hidden: bool,
    /// Width of the claude pane as a percentage of the content area in Split mode.
    pub claude_pct: u16,
    /// When true and the shell pane is focused, wrk's global Alt+… / Ctrl+Space
    /// shortcuts are not intercepted — every key (except F12, which toggles
    /// this flag) is forwarded to the shell PTY. Lets nested apps like tmux,
    /// zellij, and vim keep their own shortcuts. Persisted per project.
    pub shell_passthrough: bool,
    /// Transient text-selection mode. While set, mouse-drag selects text in
    /// the focused pane and mouse-up copies via OSC 52, then auto-exits.
    /// Escape exits without copying.
    pub select_mode: bool,
    /// While in select mode, which pane the mouse-down landed on. Drag and
    /// up events route to this pane regardless of where the cursor moves.
    pub select_anchor_pane: Option<Focus>,
    /// Path to this instance's IPC socket, exported to spawned PTYs as
    /// `WRK_SOCK`. `None` when the socket couldn't be created.
    pub socket_path: Option<PathBuf>,
    /// Terminal graphics-protocol picker for markdown image tabs, detected once
    /// at startup. `None` if detection failed — image blocks then render as
    /// their placeholder line.
    pub picker: Option<wrk_markdown::Picker>,
    /// Whether the terminal reported a dark background (detected alongside
    /// `picker`). New markdown tabs use it to auto-theme diagrams so they stay
    /// legible against the terminal color.
    pub prefers_dark: bool,
    /// Whether markdown H1–H3 headings render as true-size images (from
    /// `[markdown] heading_images`, default true). Copied onto each markdown tab
    /// at open.
    pub heading_images: bool,
    /// Active fullscreen image viewer (zoom/pan), opened from a markdown tab.
    /// When `Some`, it takes over the screen and input until dismissed.
    pub image_viewer: Option<wrk_markdown::ImageViewer>,
    /// Active code-review overlay (started by `wrk review`). When `Some`, it
    /// takes over the screen and input until dismissed.
    pub review: Option<review::ReviewSession>,
}

impl App {
    fn new(store: ProjectStore, settings: Settings) -> Self {
        let mut sidebar = ui::projects::ProjectSidebar::default();
        sidebar.refresh(&store);
        let theme = settings.theme.resolve();
        let md_theme = settings.markdown.resolve();
        let heading_images = settings.markdown.heading_images.unwrap_or(true);
        let (keymap, keymap_warnings) = KeyMap::build(&settings.keys.global);
        // Surface any keymap warnings (invalid bindings, conflicts) via the
        // status-bar error slot; they remain visible until the user dismisses
        // them by triggering any other action that overwrites `app.error`.
        let mut error = None;
        for w in keymap_warnings {
            push_error(&mut error, w);
        }
        Self {
            store,
            settings,
            theme,
            md_theme,
            keymap,
            sidebar,
            focus: Focus::Projects,
            sessions: HashMap::new(),
            active_project_name: None,
            modal: None,
            should_quit: false,
            error,
            info: None,
            last_click: None,
            layout_mode: LayoutMode::Split,
            sidebar_hidden: false,
            claude_pct: 50,
            shell_passthrough: false,
            select_mode: false,
            select_anchor_pane: None,
            socket_path: None,
            picker: None,
            prefers_dark: false,
            heading_images,
            image_viewer: None,
            review: None,
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

    /// The active project's currently-shown markdown tab, if that's what's up.
    fn active_markdown(&self) -> Option<&MarkdownTab> {
        match self.active_session()?.current()? {
            Tab::Markdown(md) => Some(md),
            Tab::Claude(_) => None,
        }
    }
    fn active_markdown_mut(&mut self) -> Option<&mut MarkdownTab> {
        match self.active_session_mut()?.current_mut()? {
            Tab::Markdown(md) => Some(md),
            Tab::Claude(_) => None,
        }
    }

    /// Environment common to every PTY wrk spawns for a project: `WRK_PROJECT`
    /// (so `wrk view` can name its originating project) and `WRK_SOCK` (this
    /// instance's IPC socket). Claude tabs additionally get `WRK_TAB`.
    fn base_pty_env(&self, project_name: &str) -> Vec<(String, String)> {
        let mut env = vec![("WRK_PROJECT".to_string(), project_name.to_string())];
        if let Some(sock) = &self.socket_path {
            env.push(("WRK_SOCK".to_string(), sock.to_string_lossy().into_owned()));
        }
        env
    }

    fn open_selected(&mut self, body: Rect) {
        let Some(idx) = self.sidebar.selected_store_index() else {
            return;
        };
        let project = self.store.projects[idx].clone();
        self.active_project_name = Some(project.name.clone());
        self.sidebar.active = Some(project.name.clone());
        self.layout_mode = project.layout_mode.unwrap_or_default();
        self.shell_passthrough = project.shell_passthrough.unwrap_or(false);
        self.error = None;

        let layout = compute_layout(body, self);
        let claude_inner = layout.claude_inner();
        let shell_inner = layout.shell_inner();
        let base_env = self.base_pty_env(&project.name);

        let session = self.sessions.entry(project.name.clone()).or_default();

        // Build the initial set of tabs from the project's configured sessions.
        // If the list is empty, create one new session named after the project.
        // A tab with no `session_id` (empty project, or a hand-written entry)
        // gets a fresh UUID assigned at spawn time and is persisted, so every
        // later open resumes deterministically via `--resume <id>`.
        if session.tabs.is_empty() {
            if project.claude_sessions.is_empty() {
                session.tabs.push(Tab::Claude(ClaudeTab {
                    name: project.name.clone(),
                    session_id: None,
                    status_id: new_status_id(),
                    status: status::TabStatus::default(),
                    pane: None,
                }));
            } else {
                for sr in &project.claude_sessions {
                    session.tabs.push(Tab::Claude(ClaudeTab {
                        name: sr.name.clone(),
                        session_id: sr.session_id.clone(),
                        status_id: new_status_id(),
                        status: status::TabStatus::default(),
                        pane: None,
                    }));
                }
            }
        }

        // Spawn any missing or dead claude panes. Tabs without a session_id are
        // assigned a fresh UUID here (`--session-id`) and persisted afterward.
        let claude_content = claude_pane_split(claude_inner, session.tabs.len()).1;
        let mut assigned_new = false;
        for tab in session.claude_tabs_mut() {
            let dead = tab.pane.as_mut().is_some_and(|p| p.child_finished());
            if tab.pane.is_none() || dead {
                let cmd = match tab.session_id.clone() {
                    Some(id) => claude_resume_command(&self.settings, &id),
                    None => {
                        let id = new_session_id();
                        let cmd = claude_new_command(&self.settings, &id, &tab.name);
                        tab.session_id = Some(id);
                        assigned_new = true;
                        cmd
                    }
                };
                let mut env = base_env.clone();
                env.push(("WRK_TAB".to_string(), tab.status_id.clone()));
                let spawned = PtyPane::spawn(
                    &cmd,
                    &project.path,
                    claude_content.height,
                    claude_content.width,
                    &env,
                );
                match spawned {
                    Ok(p) => tab.pane = Some(p),
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
                &base_env,
            ) {
                Ok(p) => Some(p),
                Err(e) => {
                    push_error(&mut self.error, format!("shell spawn failed: {e}"));
                    None
                }
            };
        }

        // Resize live panes to current geometry (covers terminal resize while inactive).
        for tab in session.claude_tabs_mut() {
            if let Some(p) = tab.pane.as_mut() {
                let _ = p.resize(claude_content.height, claude_content.width);
            }
        }
        if let Some(p) = session.shell.as_mut() {
            let _ = p.resize(shell_inner.height, shell_inner.width);
        }

        // Persist any freshly-assigned session IDs so the next open resumes them.
        if assigned_new {
            self.persist_claude_sessions(&project.name);
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

        // Re-sync per-project state from the active project (these may have
        // been edited externally).
        if let Some(p) = self.active_project() {
            let layout = p.layout_mode.unwrap_or_default();
            let passthru = p.shell_passthrough.unwrap_or(false);
            self.layout_mode = layout;
            self.shell_passthrough = passthru;
        }
        Ok(())
    }

    /// Add a new Claude tab to the active project.
    /// `session_id = None` → new session: a fresh UUID is generated up front
    /// (`claude --session-id <uuid> --name <name>`) and recorded immediately.
    /// `session_id = Some(id)` → `claude --resume <id>`.
    fn add_claude_tab(&mut self, name: String, session_id: Option<String>, body: Rect) {
        let Some(project_name) = self.active_project_name.clone() else {
            return;
        };
        let layout = compute_layout(body, self);
        let claude_inner = layout.claude_inner();
        // After this spawn there will be at least one tab → strip will render →
        // size the PTY to the post-strip content height.
        let existing = self
            .sessions
            .get(&project_name)
            .map(|s| s.tabs.len())
            .unwrap_or(0);
        let claude_content = claude_pane_split(claude_inner, existing + 1).1;

        let Some(project_path) = self.active_project().map(|p| p.path.clone()) else {
            return;
        };

        // Resume an existing session, or mint a new one with a fresh UUID so it
        // is tracked deterministically from the very first spawn.
        let (cmd, session_id) = match session_id {
            Some(id) => (claude_resume_command(&self.settings, &id), Some(id)),
            None => {
                let id = new_session_id();
                let cmd = claude_new_command(&self.settings, &id, &name);
                (cmd, Some(id))
            }
        };
        let status_id = new_status_id();
        let mut env = self.base_pty_env(&project_name);
        env.push(("WRK_TAB".to_string(), status_id.clone()));

        let pane = match PtyPane::spawn(
            &cmd,
            &project_path,
            claude_content.height,
            claude_content.width,
            &env,
        ) {
            Ok(p) => Some(p),
            Err(e) => {
                push_error(&mut self.error, format!("claude spawn failed: {e}"));
                None
            }
        };

        let session = self.sessions.entry(project_name.clone()).or_default();
        session.tabs.push(Tab::Claude(ClaudeTab {
            name,
            session_id,
            status_id,
            status: status::TabStatus::default(),
            pane,
        }));
        let new_idx = session.tabs.len() - 1;
        session.active_tab = new_idx;

        // Persist to store.
        self.persist_claude_sessions(&project_name);
    }

    /// Open a markdown file as a new tab in the active project's primary pane.
    /// `input` is resolved relative to the project directory (or used as-is if
    /// absolute). On success the new tab becomes active and the primary pane is
    /// focused. Markdown tabs are ephemeral — never persisted to `projects.toml`.
    fn open_markdown_tab(&mut self, input: &str) -> Result<()> {
        let project_name = self
            .active_project_name
            .clone()
            .ok_or_else(|| anyhow!("no active project — open one first"))?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(anyhow!("enter a file path"));
        }
        let raw = expand_tilde(trimmed);
        let joined = if raw.is_absolute() {
            raw
        } else if let Some(base) = self.active_project().map(|p| p.path.clone()) {
            base.join(raw)
        } else {
            raw
        };
        let path = joined
            .canonicalize()
            .with_context(|| format!("resolving {}", joined.display()))?;
        self.add_markdown_tab(&project_name, path)
    }

    /// Terminal cell size in pixels `(width, height)` from the graphics picker,
    /// used to size heading images. Falls back to a typical cell when no picker
    /// was detected (headings then render as their text fallback anyway).
    fn cell_size(&self) -> (u16, u16) {
        self.picker
            .as_ref()
            .map(|p| {
                let f = p.font_size();
                (f.width, f.height)
            })
            .unwrap_or((8, 16))
    }

    /// Add a markdown tab for an already-resolved absolute `path` to the given
    /// project's session (must be loaded). If that project is the active one,
    /// focus the new tab. Shared by the modal (`open_markdown_tab`) and IPC.
    fn add_markdown_tab(&mut self, project_name: &str, path: PathBuf) -> Result<()> {
        let tab = build_markdown_tab(
            path,
            self.md_theme,
            self.prefers_dark,
            self.heading_images,
            self.cell_size(),
        )?;
        let session = self
            .sessions
            .get_mut(project_name)
            .ok_or_else(|| anyhow!("project '{project_name}' is not loaded"))?;
        session.tabs.push(tab);
        session.active_tab = session.tabs.len() - 1;
        if self.active_project_name.as_deref() == Some(project_name) {
            self.focus = Focus::Claude;
        }
        Ok(())
    }

    /// Handle an [`ipc::OpenRequest`] from `wrk view`: open the file in its
    /// originating project (or the active one when unspecified). Errors surface
    /// in the status bar rather than interrupting the session.
    fn handle_open_request(&mut self, req: ipc::OpenRequest) {
        let project = req.project.or_else(|| self.active_project_name.clone());
        let Some(project) = project else {
            push_error(&mut self.error, "wrk view: no active project".into());
            return;
        };
        if let Err(e) = self.add_markdown_tab(&project, PathBuf::from(&req.path)) {
            push_error(&mut self.error, format!("wrk view: {e}"));
        }
    }

    /// Route an [`ipc::Request`] drained from the socket to its handler.
    fn handle_request(&mut self, req: ipc::Request) {
        match req {
            ipc::Request::Open(open) => self.handle_open_request(open),
            ipc::Request::Status(update) => self.handle_status_update(update),
            ipc::Request::Review(review) => self.handle_review_request(review),
        }
    }

    /// Handle a `wrk review` request: `Start` computes the diff for the named
    /// project and opens the full-screen review overlay; `End` closes it. Errors
    /// surface in the status bar rather than interrupting the session.
    fn handle_review_request(&mut self, req: ipc::ReviewRequest) {
        match req.kind {
            ipc::ReviewKind::Start { target } => {
                let project = req.project.or_else(|| self.active_project_name.clone());
                let Some(project) = project else {
                    push_error(&mut self.error, "wrk review: no active project".into());
                    return;
                };
                let Some(path) = self.store.find(&project).map(|p| p.path.clone()) else {
                    push_error(
                        &mut self.error,
                        format!("wrk review: unknown project '{project}'"),
                    );
                    return;
                };
                let target = review::diff::DiffTarget::parse(target.as_deref().unwrap_or(""));
                match review::diff::build_review(&path, &target) {
                    Ok(files) => {
                        let session = review::ReviewSession::new(project, req.tab, target, files);
                        // Clear any stale mirror from a prior review of this tab.
                        review::remove_mirror(&review_mirror_key(&session));
                        self.review = Some(session);
                    }
                    Err(e) => push_error(&mut self.error, format!("wrk review: {e}")),
                }
            }
            ipc::ReviewKind::End => {
                if let Some(session) = &self.review {
                    review::remove_mirror(&review_mirror_key(session));
                }
                self.review = None;
            }
        }
    }

    /// Mirror the active review's comments to disk so `wrk review end` (a
    /// separate process) can read them back and report them to Claude.
    fn persist_review_comments(&self) {
        if let Some(review) = &self.review {
            let _ = review::write_mirror(&review_mirror_key(review), &review.comment_records());
        }
    }

    /// Apply a hook-pushed [`ipc::StatusUpdate`]: find the Claude tab whose
    /// `status_id` matches `update.tab` (across all loaded sessions) and fold the
    /// transition into its status. Unknown tab ids are ignored — a stale push
    /// from a tab that was closed simply has no target.
    fn handle_status_update(&mut self, update: ipc::StatusUpdate) {
        for session in self.sessions.values_mut() {
            for tab in session.claude_tabs_mut() {
                if tab.status_id == update.tab {
                    tab.status.apply(update.kind);
                    return;
                }
            }
        }
    }

    /// Close the currently active tab. Markdown tabs are closed freely; a Claude
    /// tab is only closed when another Claude tab remains, so a project always
    /// keeps at least one Claude session. Persists only when a Claude tab was
    /// removed (markdown tabs aren't tracked in `projects.toml`).
    fn close_active_tab(&mut self) {
        let Some(project_name) = self.active_project_name.clone() else {
            return;
        };
        let Some(session) = self.sessions.get_mut(&project_name) else {
            return;
        };
        let Some(tab) = session.current() else {
            return;
        };
        let removed_claude = matches!(tab, Tab::Claude(_));
        if removed_claude && session.claude_tabs().count() <= 1 {
            return;
        }
        session.tabs.remove(session.active_tab);
        if session.active_tab >= session.tabs.len() {
            session.active_tab = session.tabs.len().saturating_sub(1);
        }
        if removed_claude {
            self.persist_claude_sessions(&project_name);
        }
    }

    /// Unload a project's running session: tear down its Claude tabs and shell
    /// PTYs and drop all in-memory state so the project is as if it had never
    /// been opened. The project stays in the store (`d` deletes from config).
    ///
    /// Killing the child processes and joining the reader threads happens via
    /// `PtyPane`'s `Drop` when the `ProjectSession` is removed from the map.
    /// Returns `false` if the project had no live session (nothing to unload).
    /// Open the fullscreen zoom/pan viewer for `source`. No-op without a
    /// graphics protocol (nothing could be drawn); sets an info hint if the
    /// image can't be decoded.
    fn open_image_viewer(&mut self, source: &wrk_markdown::ImageSource) {
        if self.picker.is_none() {
            return;
        }
        match wrk_markdown::ImageViewer::open(source) {
            Some(v) => self.image_viewer = Some(v),
            None => self.info = Some("could not open image".to_string()),
        }
    }

    /// Request application exit. Opens a confirmation modal when `confirm_quit`
    /// is enabled (the default); otherwise quits immediately. A modal already
    /// being open (e.g. the confirm dialog itself) is left untouched.
    fn request_quit(&mut self) {
        if self.settings.confirm_quit {
            if self.modal.is_none() {
                self.modal = Some(ModalState::ConfirmQuit(ConfirmQuitModal));
            }
        } else {
            self.should_quit = true;
        }
    }

    fn unload_project(&mut self, name: &str) -> bool {
        let Some(session) = self.sessions.remove(name) else {
            return false;
        };
        // Dropping `session` here kills its child processes and joins their
        // reader threads (see `PtyPane`'s Drop impl). In-memory status goes with
        // it — there are no on-disk status files to clean up.
        drop(session);

        // If we just unloaded the active project, return to the empty state so
        // the content area shows the placeholder instead of a dead session.
        if self.active_project_name.as_deref() == Some(name) {
            self.active_project_name = None;
            self.sidebar.active = None;
            self.focus = Focus::Projects;
        }
        true
    }

    fn next_tab(&mut self) {
        let Some(session) = self.active_session_mut() else {
            return;
        };
        if session.tabs.len() > 1 {
            session.active_tab = (session.active_tab + 1) % session.tabs.len();
        }
    }

    fn prev_tab(&mut self) {
        let Some(session) = self.active_session_mut() else {
            return;
        };
        if session.tabs.len() > 1 {
            let n = session.tabs.len();
            session.active_tab = (session.active_tab + n - 1) % n;
        }
    }

    /// Write the current set of Claude tabs back to the project store and save.
    fn persist_claude_sessions(&mut self, project_name: &str) {
        let tabs = match self.sessions.get(project_name) {
            Some(s) => s
                .claude_tabs()
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
            // Persist the live tab list faithfully. A tab keeps `session_id =
            // None` only if it hasn't been spawned yet; once spawned it always
            // has a UUID (assigned at spawn time), so restarts resume it.
            p.claude_sessions = tabs;
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

    /// Toggle shell-pane passthrough for the active project and persist the
    /// new value. With no active project this is a no-op (nothing to persist
    /// against).
    fn toggle_shell_passthrough(&mut self) {
        let new = !self.shell_passthrough;
        self.shell_passthrough = new;
        let Some(name) = self.active_project_name.clone() else {
            return;
        };
        // Store `None` instead of `Some(false)` so we don't write a noisy
        // `passthrough = false` line into projects.toml for the default case.
        let to_store = if new { Some(true) } else { None };
        let mut changed = false;
        if let Some(p) = self.store.projects.iter_mut().find(|p| p.name == name)
            && p.shell_passthrough != to_store
        {
            p.shell_passthrough = to_store;
            changed = true;
        }
        if changed && let Err(e) = store::save(&self.store) {
            push_error(&mut self.error, format!("save failed: {e}"));
        }
    }
}

/// A fresh Claude session UUID (v4) for a brand-new tab.
fn new_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Build the command to **resume** an existing Claude session: `claude --resume
/// <id>`.
///
/// `claude --continue` is intentionally not used. It resumes "the most recent
/// session in this directory", which is non-deterministic when multiple wrk
/// projects share a path: project A could end up attached to a session that
/// actually belongs to project B simply because B was used more recently.
fn claude_resume_command(settings: &Settings, session_id: &str) -> Vec<String> {
    let mut cmd = settings.claude_base();
    cmd.push("--resume".to_string());
    cmd.push(session_id.to_string());
    cmd
}

/// Build the command to **create** a new Claude session with a caller-chosen
/// UUID and display name: `claude --session-id <id> --name <name>`.
///
/// Assigning the ID up front (rather than guessing it from the filesystem after
/// the fact) is what makes resumption deterministic — the tab is bound to its
/// own session from the very first launch, so it can never latch onto another
/// project's conversation. The `--name` labels the session after its project
/// (shown in Claude's `/resume` picker and terminal title).
fn claude_new_command(settings: &Settings, session_id: &str, name: &str) -> Vec<String> {
    let mut cmd = settings.claude_base();
    cmd.push("--session-id".to_string());
    cmd.push(session_id.to_string());
    cmd.push("--name".to_string());
    cmd.push(name.to_string());
    cmd
}

/// Read a markdown file at an absolute `path` into a [`Tab`]. Rendering is
/// deferred to the first draw, when the pane width is known (see
/// [`MarkdownTab::ensure_rendered`]).
fn build_markdown_tab(
    path: PathBuf,
    theme: wrk_markdown::MdTheme,
    prefers_dark: bool,
    heading_images: bool,
    cell_size: (u16, u16),
) -> Result<Tab> {
    let source =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("markdown")
        .to_string();
    Ok(Tab::Markdown(MarkdownTab {
        name,
        path,
        source,
        rendered: wrk_markdown::RenderedDoc::default(),
        render_width: 0,
        theme,
        diagram_ctx: wrk_markdown::DiagramCtx {
            prefers_dark,
            opaque_background: false,
        },
        heading_images,
        cell_size,
        state: wrk_markdown::MarkdownViewState::new(),
    }))
}

fn push_error(slot: &mut Option<String>, msg: String) {
    *slot = Some(match slot.take() {
        Some(prev) => format!("{prev}; {msg}"),
        None => msg,
    });
}

/// Copy `text` to the host terminal's clipboard via OSC 52 (`ESC ]52;c;<b64> BEL`).
/// Self-contained so it works over SSH without needing `xclip`/`wl-copy`. Some
/// terminals require enabling OSC 52 clipboard writes (e.g. xterm
/// `allowWindowOps`/`disallowedWindowOps`, tmux `set -g allow-passthrough on`).
fn copy_to_clipboard(text: &str) {
    use base64::Engine;
    use std::io::Write;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let payload = format!("\x1b]52;c;{encoded}\x07");
    let mut stdout = io::stdout();
    let _ = stdout.write_all(payload.as_bytes());
    let _ = stdout.flush();
}

fn run_tui() -> Result<()> {
    let store = store::load()?;
    let settings = settings::load().unwrap_or_else(|e| {
        eprintln!("warning: failed to load settings.toml: {e}; using defaults");
        Settings::default()
    });
    let mut app = App::new(store, settings);

    // IPC socket for `wrk view` and `wrk hook` run inside a pane. Non-fatal if
    // it can't bind — `wrk view` falls back to the pager and hook status pushes
    // are simply dropped.
    let (socket_path, ipc_rx) = match ipc::serve() {
        Ok((p, rx)) => (Some(p), Some(rx)),
        Err(e) => {
            eprintln!("warning: IPC socket unavailable ({e}); `wrk view` will use the pager");
            (None, None)
        }
    };
    app.socket_path = socket_path.clone();

    enable_raw_mode().context("enabling raw mode")?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
    )
    .context("entering alternate screen")?;
    // Detect the terminal's graphics protocol for markdown image tabs. Must run
    // after entering the alternate screen (per ratatui-image) and before the
    // event loop; failure leaves `picker` None so images show placeholders.
    app.picker = wrk_markdown::query_picker();
    // The same query reports the terminal background; use it to auto-theme
    // diagrams (dark terminal → dark diagram palette). Defaults to light when
    // the terminal doesn't answer.
    app.prefers_dark = app
        .picker
        .as_ref()
        .and_then(wrk_markdown::terminal_prefers_dark)
        .unwrap_or(false);
    // Ask the host terminal for disambiguated key reporting (Kitty Keyboard
    // Protocol). On supporting terminals this is what lets us tell Shift+Enter
    // apart from plain Enter; on terminals that don't support it the request
    // is silently ignored. Tracked separately so we know whether to pop on
    // shutdown.
    // DISAMBIGUATE_ESCAPE_CODES alone makes kitty-protocol terminals report
    // shifted printable chars as their *base* codepoint plus a SHIFT modifier
    // (e.g. `Shift+,` arrives as `Char(',')` + SHIFT instead of `Char('<')`).
    // That broke our `Alt+<` / `Alt+>` claude-tab shortcuts. REPORT_ALTERNATE_KEYS
    // tells the terminal to also send the shifted form; crossterm's parser then
    // promotes that to the keycode and drops the SHIFT modifier, so the rest of
    // wrk sees the same `Char('<')` events it always did.
    let pushed_kbd_flags = execute!(
        stdout,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    )
    .is_ok();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("creating terminal")?;

    let watch_rx = spawn_watcher()?;

    let res = event_loop(&mut terminal, &mut app, watch_rx, ipc_rx);

    disable_raw_mode().ok();
    let mut stdout = io::stdout();
    if pushed_kbd_flags {
        execute!(stdout, PopKeyboardEnhancementFlags).ok();
    }
    execute!(
        stdout,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
    )
    .ok();
    terminal.show_cursor().ok();

    // Remove our IPC socket so a stale path can't linger for the next instance
    // that happens to reuse this pid.
    if let Some(p) = &socket_path {
        let _ = std::fs::remove_file(p);
    }

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
    ipc_rx: Option<Receiver<ipc::Request>>,
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
            let claude_content = claude_pane_split(claude_inner, session.tabs.len()).1;
            for tab in session.claude_tabs_mut() {
                if let Some(p) = tab.pane.as_mut() {
                    let _ = p.resize(claude_content.height, claude_content.width);
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

        // Drain IPC requests: `wrk view` open-file requests and `wrk hook`
        // status pushes.
        if let Some(rx) = &ipc_rx {
            while let Ok(req) = rx.try_recv() {
                app.handle_request(req);
            }
        }

        if event::poll(Duration::from_millis(33))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => handle_key(app, key, body)?,
                Event::Mouse(m) => handle_mouse(app, m, area),
                Event::Paste(content) => handle_paste(app, content, body),
                Event::Resize(_, _) => {
                    // Next loop iteration re-resizes.
                }
                _ => {}
            }
        }

        // Reap dead children on the active session so the placeholder shows.
        if let Some(session) = app.active_session_mut() {
            for tab in session.claude_tabs_mut() {
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

/// Splits the Claude pane's inner (post-border) area into the optional tab
/// strip (top row, height 1) and the terminal content area below it. The
/// strip is only present when there are tabs to display and the inner area
/// is at least 2 rows tall.
///
/// Shared by the renderer (`ui::draw_claude_pane`), the resize loop, the PTY
/// spawn sites, and the mouse click router so they all agree on where the
/// strip lives and how much room is left for the embedded terminal.
pub fn claude_pane_split(inner: Rect, tab_count: usize) -> (Option<Rect>, Rect) {
    if tab_count > 0 && inner.height >= 2 {
        let strip = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        };
        let content = Rect {
            x: inner.x,
            y: inner.y + 1,
            width: inner.width,
            height: inner.height - 1,
        };
        (Some(strip), content)
    } else {
        (None, inner)
    }
}

/// Diagnostic: dump the currently-focused PTY's alacritty grid to a text
/// file under `/tmp/`. The path is reported back via `app.error` so the
/// status bar shows where the file landed.
fn dump_focused_grid(app: &mut App) {
    let project = app
        .active_project_name
        .clone()
        .unwrap_or_else(|| "noproject".into());
    let (label, pane_ref): (&str, Option<&pane::terminal::PtyPane>) = match app.focus {
        Focus::Claude => ("claude", app.active_claude()),
        Focus::Shell => ("shell", app.active_shell()),
        Focus::Projects => {
            push_error(&mut app.error, "dump: focus a claude or shell pane".into());
            return;
        }
    };
    let Some(pane) = pane_ref else {
        push_error(&mut app.error, "dump: no live pane in focus".into());
        return;
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let safe_project: String = project
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let path = std::path::PathBuf::from(format!("/tmp/wrk-dump-{ts}-{safe_project}-{label}.txt"));
    match pane.dump_grid(&path) {
        Ok(()) => {
            push_error(&mut app.error, format!("dumped grid to {}", path.display()));
        }
        Err(e) => {
            push_error(&mut app.error, format!("dump failed: {e}"));
        }
    }
}

fn handle_key(app: &mut App, key: KeyEvent, body: Rect) -> Result<()> {
    // Ephemeral status feedback (e.g. "copied N chars") clears on the next
    // user action so it doesn't shadow the hint indefinitely.
    app.info = None;

    // The fullscreen image viewer captures all input while open.
    if app.image_viewer.is_some() {
        handle_image_viewer_key(app, key);
        return Ok(());
    }

    // The fullscreen review overlay captures all input while open.
    if app.review.is_some() {
        handle_review_key(app, key);
        return Ok(());
    }

    if app.modal.is_some() {
        return handle_modal_key(app, key, body);
    }

    let action = app.keymap.lookup(&key);

    // While select mode is on, only Esc (cancel) and the select-mode trigger
    // itself (toggle off) fire. Everything else is swallowed so partial
    // input doesn't reach the underlying app while the user is dragging.
    if app.select_mode {
        if key.code == KeyCode::Esc || action == Some(GlobalAction::EnterSelectMode) {
            exit_select_mode(app);
        }
        return Ok(());
    }

    // The passthrough toggle is always honored — even while passthrough is on
    // and the shell pane is focused — so the user can always get back out.
    if action == Some(GlobalAction::ToggleShellPassthrough) {
        app.toggle_shell_passthrough();
        return Ok(());
    }

    // When passthrough is on AND the shell pane is focused, every other key
    // (whether it has a global binding or not) goes straight to the PTY so
    // nested apps (tmux, zellij, vim, …) keep their own shortcuts.
    if app.shell_passthrough && app.focus == Focus::Shell {
        forward_key_to_focused_pty(app, key);
        return Ok(());
    }

    // Try to dispatch a global action. `dispatch_global_action` returns
    // false when the action isn't applicable in the current context (e.g.
    // NewClaudeTab outside the claude pane), in which case we fall through
    // to per-focus key handling so the keystroke can still reach a PTY.
    if let Some(action) = action
        && dispatch_global_action(app, action, body)
    {
        return Ok(());
    }

    match app.focus {
        Focus::Projects => handle_projects_key(app, key, body),
        // When the active primary tab is a markdown viewer, keys drive the
        // viewer (scroll/reload) instead of being forwarded to a PTY.
        Focus::Claude if active_tab_is_markdown(app) => {
            handle_markdown_key(app, key);
            Ok(())
        }
        Focus::Claude | Focus::Shell => {
            forward_key_to_focused_pty(app, key);
            Ok(())
        }
    }
}

/// True when the active project's currently-shown primary tab is a markdown
/// viewer (so input/scroll route to the viewer rather than a PTY).
fn active_tab_is_markdown(app: &App) -> bool {
    app.active_session()
        .and_then(|s| s.current())
        .is_some_and(Tab::is_markdown)
}

/// Handle a key while a markdown tab is focused: scrolling, paging, and reload.
fn handle_markdown_key(app: &mut App, key: KeyEvent) {
    // `Enter` opens the top-most image in view in the fullscreen viewer; the
    // source is captured here and opened after the tab borrow ends.
    let mut open_source: Option<wrk_markdown::ImageSource> = None;
    {
        let Some(session) = app.active_session_mut() else {
            return;
        };
        let Some(Tab::Markdown(md)) = session.current_mut() else {
            return;
        };
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => md.state.scroll_by(1),
            KeyCode::Char('k') | KeyCode::Up => md.state.scroll_by(-1),
            KeyCode::Char('d') | KeyCode::PageDown | KeyCode::Char(' ') => md.state.page_down(),
            KeyCode::Char('u') | KeyCode::PageUp => md.state.page_up(),
            KeyCode::Char('g') | KeyCode::Home => md.state.scroll_to_top(),
            KeyCode::Char('G') | KeyCode::End => md.state.scroll_to_bottom(),
            KeyCode::Char('r') => md.reload(),
            KeyCode::Char('b') => md.toggle_diagram_background(),
            KeyCode::Enter => {
                open_source = image_source_for_block(md, md.state.top_visible_image())
            }
            _ => {}
        }
    }
    if let Some(src) = open_source {
        app.open_image_viewer(&src);
    }
}

/// The [`ImageSource`](wrk_markdown::ImageSource) of the image at document block
/// `idx` in `md`, if that block is an image.
fn image_source_for_block(
    md: &MarkdownTab,
    idx: Option<usize>,
) -> Option<wrk_markdown::ImageSource> {
    let idx = idx?;
    match md.rendered.blocks.get(idx)? {
        wrk_markdown::MdBlock::Image(img) => Some(img.source.clone()),
        wrk_markdown::MdBlock::Text(_) => None,
    }
}

/// Handle a key while the fullscreen image viewer is open: zoom, pan, reset, or
/// close.
fn handle_image_viewer_key(app: &mut App, key: KeyEvent) {
    if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
        app.image_viewer = None;
        // The fullscreen overlay clobbered the inline images' on-screen cells;
        // force the markdown view to rebuild its image protocols so they
        // re-transmit (a cached protocol would otherwise emit "skip").
        if let Some(md) = app.active_markdown_mut() {
            md.render_width = 0;
        }
        return;
    }
    let Some(v) = app.image_viewer.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Char('+') | KeyCode::Char('=') => v.zoom_by(1.25),
        KeyCode::Char('-') | KeyCode::Char('_') => v.zoom_by(0.8),
        KeyCode::Char('0') => v.reset(),
        KeyCode::Char('h') | KeyCode::Left => v.pan_view(-IMG_PAN_STEP, 0.0),
        KeyCode::Char('l') | KeyCode::Right => v.pan_view(IMG_PAN_STEP, 0.0),
        KeyCode::Char('k') | KeyCode::Up => v.pan_view(0.0, -IMG_PAN_STEP),
        KeyCode::Char('j') | KeyCode::Down => v.pan_view(0.0, IMG_PAN_STEP),
        _ => {}
    }
}

/// Keyboard pan step, as a fraction of the visible view.
const IMG_PAN_STEP: f32 = 0.2;

/// Handle a key while the full-screen code-review overlay is open. Navigation is
/// vim-ish; `q`/`Esc` closes (the review session is dropped, but comments are
/// already mirrored to disk for `wrk review end`). While the comment editor is
/// open, keys route to it instead.
fn handle_review_key(app: &mut App, key: KeyEvent) {
    use review::{ReviewFocus, Side};

    // Comment editor captures input while typing.
    if app.review.as_ref().is_some_and(|r| r.editing.is_some()) {
        let review = app.review.as_mut().unwrap();
        let mut changed = false;
        match key.code {
            KeyCode::Esc => review.cancel_comment(),
            KeyCode::Enter => changed = review.save_comment(),
            KeyCode::Backspace => review.editor_backspace(),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                review.editor_push_char(c)
            }
            _ => {}
        }
        if changed {
            app.persist_review_comments();
        }
        return;
    }

    if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
        app.review = None;
        return;
    }
    let Some(review) = app.review.as_mut() else {
        return;
    };
    let mut changed = false;
    match key.code {
        KeyCode::Tab | KeyCode::BackTab => review.toggle_focus(),
        // In the diff, ←/→ (h/l) pick the before/after comment side; in the file
        // list they don't apply.
        KeyCode::Left | KeyCode::Char('h') if review.focus == ReviewFocus::Diff => {
            review.set_side(Side::Before)
        }
        KeyCode::Right | KeyCode::Char('l') if review.focus == ReviewFocus::Diff => {
            review.set_side(Side::After)
        }
        KeyCode::Right | KeyCode::Char('l') => review.focus = ReviewFocus::Diff,
        KeyCode::Down | KeyCode::Char('j') => match review.focus {
            ReviewFocus::Files => review.select_file(1),
            ReviewFocus::Diff => review.move_cursor(1),
        },
        KeyCode::Up | KeyCode::Char('k') => match review.focus {
            ReviewFocus::Files => review.select_file(-1),
            ReviewFocus::Diff => review.move_cursor(-1),
        },
        KeyCode::PageDown | KeyCode::Char('d') => review.move_cursor(10),
        KeyCode::PageUp | KeyCode::Char('u') => review.move_cursor(-10),
        KeyCode::Char('g') => review.cursor_to_edge(true),
        KeyCode::Char('G') => review.cursor_to_edge(false),
        // Reveal the collapsed gap under the cursor; from the file list, Enter
        // jumps into the diff.
        KeyCode::Enter | KeyCode::Char(' ') => match review.focus {
            ReviewFocus::Files => review.focus = ReviewFocus::Diff,
            ReviewFocus::Diff => review.reveal_at_cursor(),
        },
        KeyCode::Char('e') => review.set_all_revealed(true),
        KeyCode::Char('o') => review.set_all_revealed(false),
        // Comment on / delete a comment from the cursor row.
        KeyCode::Char('c') if review.focus == ReviewFocus::Diff => review.begin_comment(),
        KeyCode::Char('D') if review.focus == ReviewFocus::Diff => {
            changed = review.delete_comment_at_cursor()
        }
        _ => {}
    }
    if changed {
        app.persist_review_comments();
    }
}

/// Handle a mouse event while the fullscreen image viewer is open: the wheel
/// zooms. (Pan is on the keyboard for now.)
fn handle_image_viewer_mouse(app: &mut App, m: MouseEvent) {
    if let Some(v) = app.image_viewer.as_mut() {
        match m.kind {
            MouseEventKind::ScrollUp => v.zoom_by(1.15),
            MouseEventKind::ScrollDown => v.zoom_by(1.0 / 1.15),
            _ => {}
        }
    }
}

/// If buffer cell `(x, y)` is over an inline image in the active markdown tab,
/// open it in the fullscreen viewer and return `true`.
fn try_open_image_at(app: &mut App, x: u16, y: u16) -> bool {
    if !active_tab_is_markdown(app) {
        return false;
    }
    let source = app
        .active_markdown_mut()
        .and_then(|md| image_source_for_block(md, md.state.image_at(x, y)));
    match source {
        Some(src) => {
            app.open_image_viewer(&src);
            true
        }
        None => false,
    }
}

/// Run a [`GlobalAction`]. Returns `true` if the action was applied; `false`
/// if it wasn't applicable in the current context (the caller falls through
/// to per-focus dispatch so the keystroke isn't lost).
fn dispatch_global_action(app: &mut App, action: GlobalAction, body: Rect) -> bool {
    match action {
        GlobalAction::Quit => {
            app.request_quit();
        }
        GlobalAction::FocusProjects | GlobalAction::LeaderFocusProjects => {
            app.focus = Focus::Projects;
        }
        GlobalAction::FocusClaude => {
            app.focus = Focus::Claude;
        }
        GlobalAction::FocusShell => {
            app.focus = Focus::Shell;
        }
        GlobalAction::ToggleSidebar => {
            app.sidebar_hidden = !app.sidebar_hidden;
            if app.sidebar_hidden && app.focus == Focus::Projects {
                app.focus = Focus::Claude;
            }
        }
        GlobalAction::ShrinkClaude => {
            app.claude_pct = app.claude_pct.saturating_sub(5).max(10);
        }
        GlobalAction::GrowClaude => {
            app.claude_pct = (app.claude_pct + 5).min(90);
        }
        GlobalAction::ToggleLayout => {
            let new_mode = match app.layout_mode {
                LayoutMode::Split => LayoutMode::Tabbed,
                LayoutMode::Tabbed => LayoutMode::Split,
            };
            app.set_layout_mode(new_mode);
            if app.layout_mode == LayoutMode::Tabbed && app.focus == Focus::Projects {
                app.focus = Focus::Claude;
            }
        }
        // `new_claude_tab` only fires while the claude pane is focused so it
        // doesn't intercept its bound key inside a shell app.
        GlobalAction::NewClaudeTab => {
            if app.focus != Focus::Claude || app.active_project_name.is_none() {
                return false;
            }
            let _ = body;
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
        }
        GlobalAction::CloseClaudeTab => {
            app.close_active_tab();
        }
        GlobalAction::PrevClaudeTab => {
            app.prev_tab();
        }
        GlobalAction::NextClaudeTab => {
            app.next_tab();
        }
        GlobalAction::OpenMarkdown => {
            if app.active_project_name.is_none() {
                push_error(&mut app.error, "open markdown: open a project first".into());
                return false;
            }
            app.modal = Some(ModalState::OpenMarkdown(OpenMarkdownModal::default()));
        }
        // Handled in handle_key before passthrough check.
        GlobalAction::ToggleShellPassthrough => {
            app.toggle_shell_passthrough();
        }
        GlobalAction::OpenLinkPicker => {
            return open_link_picker(app);
        }
        GlobalAction::EnterSelectMode => {
            enter_select_mode(app);
        }
        GlobalAction::DumpGrid => {
            dump_focused_grid(app);
        }
    }
    true
}

/// Scan the focused pane's scrollback for URLs and open a picker modal.
/// Returns `false` when there's no pane to scan (e.g. Projects focus) so the
/// keystroke can fall through to per-focus handling.
fn open_link_picker(app: &mut App) -> bool {
    let urls = {
        let Some(session) = app.active_session() else {
            return false;
        };
        let pane = match app.focus {
            Focus::Claude => session.active_claude_pane(),
            Focus::Shell => session.shell.as_ref(),
            Focus::Projects => return false,
        };
        let Some(pane) = pane else {
            return false;
        };
        pane.collect_urls()
    };
    if urls.is_empty() {
        push_error(&mut app.error, "no URLs in scrollback".to_string());
    } else {
        app.modal = Some(ModalState::UrlPicker(UrlPickerModal::new(urls)));
    }
    true
}

/// Enter transient text-selection mode. Drag with the mouse to select; mouse
/// up copies via OSC 52 and auto-exits. No-op when focus is on Projects (no
/// pane to select from); a hint is shown so the user knows why.
fn enter_select_mode(app: &mut App) {
    if app.focus == Focus::Projects {
        push_error(
            &mut app.error,
            "select mode: focus a claude or shell pane first".to_string(),
        );
        return;
    }
    app.select_mode = true;
    app.select_anchor_pane = None;
    // No status push here — `build_hint` already shows the right
    // "drag to select / release to copy / Esc to cancel" guidance
    // while `select_mode` is on.
}

/// Cancel select mode without copying. Clears any in-progress selection on
/// the pane (or markdown viewer) the drag had anchored to.
fn exit_select_mode(app: &mut App) {
    if let Some(focus) = app.select_anchor_pane.take() {
        if md_select_target(app, focus) {
            if let Some(md) = app.active_markdown_mut() {
                md.state.selection_clear();
            }
        } else if let Some(pane) = pane_for_focus(app, focus) {
            pane.selection_clear();
        }
    }
    app.select_mode = false;
}

/// Whether select-mode input at `focus` targets a markdown viewer (which lives
/// in the primary/claude pane) rather than a PTY.
fn md_select_target(app: &App, focus: Focus) -> bool {
    focus == Focus::Claude && active_tab_is_markdown(app)
}

/// Route a bracketed-paste payload from the host terminal. When a modal text
/// input or the sidebar filter is active, the paste is appended there; in
/// every other case it is forwarded to the focused PTY (wrapped in
/// `\e[200~ … \e[201~` markers when the inner program has bracketed-paste
/// mode enabled, so paste-aware apps like Claude see it as a single paste
/// rather than a stream of keypresses).
fn handle_paste(app: &mut App, content: String, body: Rect) {
    let _ = body;
    let payload = normalize_paste(&content);
    if payload.is_empty() {
        return;
    }

    if let Some(modal) = app.modal.as_mut() {
        match modal {
            ModalState::Add(m) => m.current_input_mut().push_str(&payload),
            ModalState::OpenMarkdown(m) => m.path_input.push_str(&payload),
            ModalState::ClaudeTabPicker(m) if m.name_focused => m.tab_name.push_str(&payload),
            ModalState::UrlPicker(m) => {
                for c in payload.chars() {
                    m.push_char(c);
                }
            }
            // ConfirmDelete is y/n only; ClaudeTabPicker with the list focused
            // has no text input to append to.
            _ => {}
        }
        return;
    }

    if app.focus == Focus::Projects {
        if let Some(filter) = app.sidebar.filter.as_mut() {
            filter.push_str(&payload);
            app.sidebar.refresh(&app.store);
        }
        return;
    }

    forward_paste_to_focused_pty(app, &payload);
}

/// Normalize a paste payload: convert `\r\n` and `\n` to `\r` (Enter), and
/// strip any embedded `\e[201~` end-marker so a hostile paste can't escape
/// bracketed-paste mode and inject control sequences into the inner program.
fn normalize_paste(s: &str) -> String {
    let stripped = s.replace("\x1b[201~", "");
    let mut out = String::with_capacity(stripped.len());
    let mut prev_cr = false;
    for c in stripped.chars() {
        match c {
            '\n' if prev_cr => {
                // CRLF: the \r was already pushed, swallow the \n.
            }
            '\n' => out.push('\r'),
            '\r' => out.push('\r'),
            _ => out.push(c),
        }
        prev_cr = c == '\r';
    }
    out
}

fn forward_paste_to_focused_pty(app: &mut App, payload: &str) {
    let focus = app.focus;
    let Some(session) = app.active_session_mut() else {
        return;
    };
    let pane = match focus {
        Focus::Claude => session.active_claude_pane_mut(),
        Focus::Shell => session.shell.as_mut(),
        Focus::Projects => return,
    };
    let Some(pane) = pane else {
        return;
    };
    let bytes: Vec<u8> = if pane.bracketed_paste_mode() {
        let mut v = Vec::with_capacity(payload.len() + 12);
        v.extend_from_slice(b"\x1b[200~");
        v.extend_from_slice(payload.as_bytes());
        v.extend_from_slice(b"\x1b[201~");
        v
    } else {
        payload.as_bytes().to_vec()
    };
    let _ = pane.write(&bytes);
    pane.scroll_to_bottom();
}

/// Forward `key` as bytes to whichever PTY currently has focus.
fn forward_key_to_focused_pty(app: &mut App, key: KeyEvent) {
    let focus = app.focus;
    let Some(session) = app.active_session_mut() else {
        return;
    };
    let pane = match focus {
        Focus::Claude => session.active_claude_pane_mut(),
        Focus::Shell => session.shell.as_mut(),
        Focus::Projects => return,
    };
    let Some(pane) = pane else {
        return;
    };
    let bytes = pane::key_to_bytes(key, pane.app_cursor_mode());
    if !bytes.is_empty() {
        let _ = pane.write(&bytes);
        pane.scroll_to_bottom();
    }
}

fn handle_projects_key(app: &mut App, key: KeyEvent, body: Rect) -> Result<()> {
    if app.sidebar.filter.is_some() {
        return handle_filter_key(app, key, body);
    }
    match key.code {
        KeyCode::Char('q') => app.request_quit(),
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
        KeyCode::Char('u') => {
            if let Some(idx) = app.sidebar.selected_store_index()
                && let Some(p) = app.store.projects.get(idx)
            {
                let name = p.name.clone();
                if app.sessions.contains_key(&name) {
                    app.modal = Some(ModalState::ConfirmUnload(ConfirmUnloadModal {
                        project_name: name,
                    }));
                } else {
                    app.info = Some(format!("'{name}' is not loaded"));
                }
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
    // Deferred so the markdown file is opened after `m`'s borrow of `app.modal`
    // ends (opening needs `&mut app`).
    let mut open_md_input: Option<String> = None;
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
                let path = expand_tilde(m.path_input.trim());
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
                        shell_passthrough: None,
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
        ModalState::OpenMarkdown(m) => match key.code {
            KeyCode::Esc => consumed_modal = Some(()),
            KeyCode::Backspace => {
                m.path_input.pop();
            }
            KeyCode::Char(c) => m.path_input.push(c),
            KeyCode::Enter => open_md_input = Some(m.path_input.clone()),
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
        ModalState::ConfirmUnload(m) => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let name = m.project_name.clone();
                if app.unload_project(&name) {
                    app.info = Some(format!("unloaded '{name}'"));
                }
                consumed_modal = Some(());
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                consumed_modal = Some(());
            }
            _ => {}
        },
        ModalState::ConfirmQuit(_) => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.should_quit = true;
                consumed_modal = Some(());
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
        ModalState::UrlPicker(m) => match key.code {
            KeyCode::Esc => consumed_modal = Some(()),
            KeyCode::Up => m.select_prev(),
            KeyCode::Down => m.select_next(),
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                m.select_prev();
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                m.select_next();
            }
            KeyCode::Backspace => m.pop_char(),
            KeyCode::Enter => {
                m.confirm();
                consumed_modal = Some(());
            }
            KeyCode::Char(c) => m.push_char(c),
            _ => {}
        },
    }
    if consumed_modal.is_some() {
        // Take the modal and dispatch on its terminal state. Most modals are
        // already finalized by the time we get here — they only need the
        // top-level state cleared. ClaudeTabPicker and UrlPicker carry a
        // post-confirm side effect (spawn a tab, spawn xdg-open).
        match app.modal.take() {
            Some(ModalState::ClaudeTabPicker(m)) if m.confirmed => {
                let session_id = m.selected_session_id().map(|s| s.to_owned());
                let name = if m.tab_name.trim().is_empty() {
                    m.suggested_name()
                } else {
                    m.tab_name.trim().to_string()
                };
                app.add_claude_tab(name, session_id, body);
            }
            Some(ModalState::UrlPicker(m)) => {
                if let Some(url) = m.confirmed_url {
                    let _ = std::process::Command::new("xdg-open")
                        .arg(&url)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn();
                }
            }
            _ => {}
        }
    }
    // Deferred markdown open: succeed → close the modal; fail → keep it open and
    // show the error inline.
    if let Some(input) = open_md_input {
        match app.open_markdown_tab(&input) {
            Ok(()) => app.modal = None,
            Err(e) => {
                if let Some(ModalState::OpenMarkdown(m)) = app.modal.as_mut() {
                    m.error = Some(e.to_string());
                }
            }
        }
    }
    Ok(())
}

const DOUBLE_CLICK_MS: u128 = 350;

const SCROLL_LINES: i32 = 3;

fn handle_mouse(app: &mut App, m: MouseEvent, area: Rect) {
    // Clear the ephemeral status before dispatching so the next mouse
    // action either replaces it (e.g. the select-mode mouse-up sets a
    // fresh "copied N chars") or leaves it cleared.
    app.info = None;

    // The fullscreen image viewer captures the mouse (wheel zooms) while open.
    if app.image_viewer.is_some() {
        handle_image_viewer_mouse(app, m);
        return;
    }

    // The fullscreen review overlay captures the mouse (wheel scrolls the diff).
    if let Some(review) = app.review.as_mut() {
        match m.kind {
            MouseEventKind::ScrollDown => review.move_cursor(3),
            MouseEventKind::ScrollUp => review.move_cursor(-3),
            _ => {}
        }
        return;
    }

    if app.modal.is_some() {
        return;
    }

    let body = body_rect(area);
    let layout = compute_layout(body, app);
    let pos_x = m.column;
    let pos_y = m.row;

    // Select mode owns the mouse: plain drag selects, mouse-up copies via
    // OSC 52 and auto-exits. PTY mouse-capture and Ctrl/Shift+click are
    // bypassed so the user can always select regardless of the inner app.
    if app.select_mode {
        handle_select_mouse(app, &layout, m, pos_x, pos_y);
        return;
    }

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

    // Claude tab strip (per-session tabs at the top of the claude pane).
    // Detect this *before* PTY forwarding so the click selects a tab instead
    // of being sent to Claude as a mouse event.
    if let Some(strip) = visible_claude_tab_strip(app, &layout)
        && rect_contains(strip, pos_x, pos_y)
    {
        if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
            handle_claude_tab_click(app, strip, pos_x);
        }
        return;
    }

    // Ctrl- or Shift+left-click on a URL takes precedence over both PTY
    // forwarding and focus-switching. Shift is offered as an alternative to
    // Ctrl because many outer terminal emulators capture Ctrl+Click for their
    // own link-handling before crossterm ever sees it.
    if let MouseEventKind::Down(MouseButton::Left) = m.kind
        && (m.modifiers.contains(KeyModifiers::CONTROL)
            || m.modifiers.contains(KeyModifiers::SHIFT))
        && try_open_url(app, &layout, pos_x, pos_y)
    {
        return;
    }

    // A left-click on an inline markdown image opens it in the fullscreen
    // viewer (before PTY forwarding — a markdown tab has no PTY anyway).
    if let MouseEventKind::Down(MouseButton::Left) = m.kind
        && try_open_image_at(app, pos_x, pos_y)
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

/// Dispatch a mouse event while [`App::select_mode`] is active.
///
/// `Down(Left)` anchors a fresh selection on whichever pane is under the
/// cursor and locks subsequent drags/ups to that pane. `Drag(Left)` extends
/// the selection (clamped to the anchored pane's content rect, so drag past
/// the edges still updates to the nearest cell). `Up(Left)` finalizes:
/// reads the selection text, OSC 52-copies it to the host clipboard, shows
/// "copied N chars" in the status bar, then auto-exits the mode.
///
/// Scroll events still page the alacritty scrollback of the pane under the
/// cursor so a user can scroll back, then drag to select content that
/// scrolled off-screen.
fn handle_select_mouse(app: &mut App, layout: &LayoutRects, m: MouseEvent, pos_x: u16, pos_y: u16) {
    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let Some((focus, inner)) = pane_at(app, layout, pos_x, pos_y) else {
                return;
            };
            let col = (pos_x - inner.x) as usize;
            let row = (pos_y - inner.y) as usize;
            app.focus = focus;
            app.select_anchor_pane = Some(focus);
            if md_select_target(app, focus) {
                if let Some(md) = app.active_markdown_mut() {
                    md.state.selection_anchor(col as u16, row as u16);
                }
            } else if let Some(pane) = pane_for_focus(app, focus) {
                pane.selection_anchor(col, row);
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let Some(focus) = app.select_anchor_pane else {
                return;
            };
            let Some(inner) = inner_rect_for_focus(app, layout, focus) else {
                return;
            };
            // Clamp the cursor to the anchored pane so dragging outside its
            // borders still extends to the edge cell.
            let col = pos_x.saturating_sub(inner.x) as usize;
            let row = pos_y.saturating_sub(inner.y) as usize;
            if md_select_target(app, focus) {
                if let Some(md) = app.active_markdown_mut() {
                    md.state.selection_update(col as u16, row as u16);
                }
            } else if let Some(pane) = pane_for_focus(app, focus) {
                pane.selection_update(col, row);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let focus = match app.select_anchor_pane.take() {
                Some(f) => f,
                None => return,
            };
            let text = if md_select_target(app, focus) {
                let t = app
                    .active_markdown()
                    .and_then(|md| md.state.selection_text());
                if let Some(md) = app.active_markdown_mut() {
                    md.state.selection_clear();
                }
                t
            } else {
                let t = pane_for_focus(app, focus).and_then(|p| p.selection_text());
                if let Some(pane) = pane_for_focus(app, focus) {
                    pane.selection_clear();
                }
                t
            };
            app.select_mode = false;
            match text {
                Some(s) => {
                    let n = s.chars().count();
                    copy_to_clipboard(&s);
                    app.info = Some(format!("copied {n} chars"));
                }
                None => {
                    // Empty drag (just a click) — exit silently.
                }
            }
        }
        MouseEventKind::ScrollUp => {
            scroll_at(app, layout, pos_x, pos_y, SCROLL_LINES);
        }
        MouseEventKind::ScrollDown => {
            scroll_at(app, layout, pos_x, pos_y, -SCROLL_LINES);
        }
        _ => {}
    }
}

/// Resolve the screen position `(x, y)` to the pane under the cursor and
/// its inner content rect (after stripping the border and the per-session
/// claude tab strip). Returns `None` for sidebar / tab-strip / gutter clicks.
fn pane_at(app: &App, layout: &LayoutRects, x: u16, y: u16) -> Option<(Focus, Rect)> {
    let (focus, outer, is_claude_pane) = match app.layout_mode {
        LayoutMode::Split => {
            if rect_contains(layout.claude, x, y) {
                (Focus::Claude, layout.claude, true)
            } else if rect_contains(layout.shell, x, y) {
                (Focus::Shell, layout.shell, false)
            } else {
                return None;
            }
        }
        LayoutMode::Tabbed => {
            if !rect_contains(layout.claude, x, y) {
                return None;
            }
            match app.focus {
                Focus::Claude => (Focus::Claude, layout.claude, true),
                Focus::Shell => (Focus::Shell, layout.claude, false),
                _ => return None,
            }
        }
    };
    let inner = if is_claude_pane {
        let count = app.active_session().map(|s| s.tabs.len()).unwrap_or(0);
        claude_pane_split(inset(outer), count).1
    } else {
        inset(outer)
    };
    if !rect_contains(inner, x, y) {
        return None;
    }
    Some((focus, inner))
}

/// Inner content rect for `focus` in the current layout, accounting for the
/// claude tab strip. Returns `None` if no pane is currently visible at that
/// focus (e.g. Tabbed mode with the other side showing).
fn inner_rect_for_focus(app: &App, layout: &LayoutRects, focus: Focus) -> Option<Rect> {
    let (outer, is_claude_pane) = match (app.layout_mode, focus) {
        (LayoutMode::Split, Focus::Claude) => (layout.claude, true),
        (LayoutMode::Split, Focus::Shell) => (layout.shell, false),
        (LayoutMode::Tabbed, Focus::Claude) if app.focus != Focus::Shell => (layout.claude, true),
        (LayoutMode::Tabbed, Focus::Shell) if app.focus == Focus::Shell => (layout.claude, false),
        _ => return None,
    };
    let inner = if is_claude_pane {
        let count = app.active_session().map(|s| s.tabs.len()).unwrap_or(0);
        claude_pane_split(inset(outer), count).1
    } else {
        inset(outer)
    };
    Some(inner)
}

fn pane_for_focus(app: &App, focus: Focus) -> Option<&PtyPane> {
    let session = app.active_session()?;
    match focus {
        Focus::Claude => session.active_claude_pane(),
        Focus::Shell => session.shell.as_ref(),
        Focus::Projects => None,
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

    // For the claude pane, the actual terminal content lives below the per-
    // session tab strip — adjust the inner rect so pane-local coords match
    // the cells the user can actually see.
    let inner = if target_focus == Focus::Claude {
        let count = app.active_session().map(|s| s.tabs.len()).unwrap_or(0);
        claude_pane_split(inset(outer), count).1
    } else {
        inset(outer)
    };
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
    // Per the X11 mouse spec all three reporting modes report button events:
    // `\e[?1000h` (MOUSE_REPORT_CLICK) is press/release, `\e[?1002h`
    // (MOUSE_DRAG) is press/release plus motion-while-button-held, and
    // `\e[?1003h` (MOUSE_MOTION) is press/release plus all motion. Alacritty
    // stores those as mutually exclusive flags (term/mod.rs:1953-1968), so a
    // program enabling 1002 (helix does this) clears MOUSE_REPORT_CLICK and
    // sets only MOUSE_DRAG. We therefore gate Down/Up on `mode.any()` rather
    // than `mode.report_click` — otherwise the Down to a helix pane is
    // dropped and the program sees a Drag without an anchor (#13).
    //
    // `Down` is also routed regardless of focus_match so the *first* click on
    // an unfocused mouse-aware pane reaches the program; we then switch focus
    // to that pane as a side effect so subsequent Up/Drag/Moved naturally
    // match focus.
    let should_forward = match m.kind {
        MouseEventKind::Down(_) => mode.any(),
        MouseEventKind::Up(_) => mode.any() && focus_match,
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

    // Move focus to the target pane on Down so subsequent events are routed
    // there. Done before re-borrowing the session map mutably below.
    if matches!(m.kind, MouseEventKind::Down(_)) && !focus_match {
        app.focus = target_focus;
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

fn scroll_at(app: &mut App, layout: &LayoutRects, x: u16, y: u16, delta: i32) {
    // Which logical pane is the cursor over? In Tabbed mode only the focused
    // side is visible in the shared content rect.
    let (over_primary, over_shell) = match app.layout_mode {
        LayoutMode::Split => (
            rect_contains(layout.claude, x, y),
            rect_contains(layout.shell, x, y),
        ),
        LayoutMode::Tabbed => {
            let on = rect_contains(layout.claude, x, y);
            (
                on && app.focus == Focus::Claude,
                on && app.focus == Focus::Shell,
            )
        }
    };

    // A markdown tab scrolls its view state (wheel-up, positive delta like PTY
    // scrollback, moves the view toward the top → decreasing offset).
    if over_primary && active_tab_is_markdown(app) {
        if let Some(session) = app.active_session_mut()
            && let Some(Tab::Markdown(md)) = session.current_mut()
        {
            md.state.scroll_by(-delta);
        }
        return;
    }

    let Some(session) = app.active_session() else {
        return;
    };
    let pane = if over_primary {
        session.active_claude_pane()
    } else if over_shell {
        session.shell.as_ref()
    } else {
        None
    };
    if let Some(p) = pane {
        p.scroll(delta);
    }
}

fn try_open_url(app: &App, layout: &LayoutRects, pos_x: u16, pos_y: u16) -> bool {
    let Some(session) = app.active_session() else {
        return false;
    };
    // is_claude_pane drives whether we strip off the per-session tab strip row
    // when computing the inner content rect — the shell pane has no strip even
    // when it shares the claude rect in Tabbed mode.
    let (pane, outer, is_claude_pane) = match app.layout_mode {
        LayoutMode::Split => {
            if rect_contains(layout.claude, pos_x, pos_y) {
                (session.active_claude_pane(), layout.claude, true)
            } else if rect_contains(layout.shell, pos_x, pos_y) {
                (session.shell.as_ref(), layout.shell, false)
            } else {
                return false;
            }
        }
        LayoutMode::Tabbed => {
            if !rect_contains(layout.claude, pos_x, pos_y) {
                return false;
            }
            match app.focus {
                Focus::Claude => (session.active_claude_pane(), layout.claude, true),
                Focus::Shell => (session.shell.as_ref(), layout.claude, false),
                _ => return false,
            }
        }
    };
    let Some(pane) = pane else {
        return false;
    };
    let inner = if is_claude_pane {
        claude_pane_split(inset(outer), session.tabs.len()).1
    } else {
        inset(outer)
    };
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

/// Returns the rect occupied by the Claude pane's per-session tab strip if it
/// is currently rendered. The strip is on top of the claude inner area when
/// there is at least one tab and the claude pane is visible (always in Split,
/// only when focus is not Shell in Tabbed).
fn visible_claude_tab_strip(app: &App, layout: &LayoutRects) -> Option<Rect> {
    let session = app.active_session()?;
    let count = session.tabs.len();
    if count == 0 {
        return None;
    }
    if app.layout_mode == LayoutMode::Tabbed && app.focus == Focus::Shell {
        return None;
    }
    claude_pane_split(inset(layout.claude), count).0
}

/// Switches the active Claude tab to the one under the click `x`, and gives
/// the Claude pane focus.
fn handle_claude_tab_click(app: &mut App, strip: Rect, x: u16) {
    let Some(session) = app.active_session_mut() else {
        return;
    };
    let n = session.tabs.len();
    if n == 0 || strip.width == 0 || x < strip.x {
        return;
    }
    let tab_width = (strip.width as usize / n).max(1) as u16;
    let mut idx = ((x - strip.x) / tab_width) as usize;
    if idx >= n {
        idx = n - 1;
    }
    session.active_tab = idx;
    app.focus = Focus::Claude;
}

fn rect_contains(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

#[cfg(test)]
mod tests {
    use super::{
        App, ClaudeTab, Focus, MarkdownTab, ModalState, Project, ProjectSession, ProjectStore,
        Settings, Tab, claude_new_command, claude_resume_command, expand_tilde, new_session_id,
        normalize_paste,
    };
    use std::path::PathBuf;

    #[test]
    fn request_quit_confirms_by_default() {
        let mut app = empty_app();
        assert!(app.settings.confirm_quit, "confirm_quit defaults on");
        app.request_quit();
        assert!(!app.should_quit, "must not quit before confirmation");
        assert!(
            matches!(app.modal, Some(ModalState::ConfirmQuit(_))),
            "expected the quit-confirmation modal"
        );
        // A second request while the modal is up is a no-op (no stacking).
        app.request_quit();
        assert!(matches!(app.modal, Some(ModalState::ConfirmQuit(_))));
    }

    #[test]
    fn request_quit_immediate_when_disabled() {
        let mut app = empty_app();
        app.settings.confirm_quit = false;
        app.request_quit();
        assert!(
            app.should_quit,
            "should quit immediately when confirm is off"
        );
        assert!(app.modal.is_none(), "no modal when confirm is off");
    }

    fn empty_app() -> App {
        App::new(ProjectStore::default(), Settings::default())
    }

    #[test]
    fn expand_tilde_resolves_home_prefix_only() {
        let Ok(home) = std::env::var("HOME") else {
            return; // no HOME in this environment; nothing to assert
        };
        let home = PathBuf::from(home);
        // `~` and `~/…` expand to the home directory.
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/test.md"), home.join("test.md"));
        assert_eq!(expand_tilde("~/a/b.md"), home.join("a/b.md"));
        // Absolute and relative paths are untouched.
        assert_eq!(expand_tilde("/abs/x.md"), PathBuf::from("/abs/x.md"));
        assert_eq!(expand_tilde("rel/x.md"), PathBuf::from("rel/x.md"));
        // A `~` that isn't the home prefix stays literal (no `~user` expansion,
        // no mid-path expansion).
        assert_eq!(expand_tilde("~bob/x"), PathBuf::from("~bob/x"));
        assert_eq!(expand_tilde("/a/~/b"), PathBuf::from("/a/~/b"));
    }

    #[test]
    fn resume_command_uses_resume_flag() {
        let s = Settings::default();
        assert_eq!(
            claude_resume_command(&s, "abc-123"),
            vec!["claude", "--resume", "abc-123"]
        );
    }

    #[test]
    fn new_command_pins_session_id_and_name() {
        let s = Settings::default();
        assert_eq!(
            claude_new_command(&s, "abc-123", "my project"),
            vec!["claude", "--session-id", "abc-123", "--name", "my project"]
        );
    }

    #[test]
    fn command_builders_honor_the_claude_base_wrapper() {
        // A wrapper command (e.g. NixOS `steam-run`) with a trailing --continue
        // that claude_base strips: both builders prepend the wrapper and append
        // their own session flags.
        let s = Settings {
            claude_command: vec![
                "steam-run".to_string(),
                "claude".to_string(),
                "--continue".to_string(),
            ],
            ..Settings::default()
        };
        assert_eq!(
            claude_resume_command(&s, "id1"),
            vec!["steam-run", "claude", "--resume", "id1"]
        );
        assert_eq!(
            claude_new_command(&s, "id1", "proj"),
            vec![
                "steam-run",
                "claude",
                "--session-id",
                "id1",
                "--name",
                "proj"
            ]
        );
    }

    #[test]
    fn new_session_id_is_a_unique_uuid() {
        let a = new_session_id();
        let b = new_session_id();
        assert_ne!(a, b);
        // Canonical UUID: 36 chars, hyphens in the 8-4-4-4-12 positions.
        assert_eq!(a.len(), 36);
        assert!(uuid::Uuid::parse_str(&a).is_ok());
    }

    fn claude_tab(name: &str) -> Tab {
        Tab::Claude(ClaudeTab {
            name: name.to_string(),
            session_id: None,
            status_id: String::new(),
            status: crate::status::TabStatus::default(),
            pane: None,
        })
    }

    fn md_tab(name: &str) -> Tab {
        Tab::Markdown(MarkdownTab {
            name: name.to_string(),
            path: PathBuf::from(name),
            source: "x".to_string(),
            rendered: wrk_markdown::RenderedDoc::default(),
            render_width: 0,
            theme: wrk_markdown::MdTheme::default(),
            diagram_ctx: wrk_markdown::DiagramCtx::default(),
            heading_images: false,
            cell_size: (8, 16),
            state: wrk_markdown::MarkdownViewState::new(),
        })
    }

    /// An app with one active project "p" whose session holds `tabs`.
    fn app_with_tabs(tabs: Vec<Tab>) -> App {
        let mut app = empty_app();
        let name = "p".to_string();
        app.active_project_name = Some(name.clone());
        let session = ProjectSession {
            tabs,
            ..Default::default()
        };
        app.sessions.insert(name, session);
        app
    }

    #[test]
    fn claude_tabs_iterator_skips_markdown() {
        let session = ProjectSession {
            tabs: vec![claude_tab("a"), md_tab("m"), claude_tab("b")],
            ..Default::default()
        };
        assert_eq!(session.claude_tabs().count(), 2);
        let names: Vec<_> = session.claude_tabs().map(|t| t.name.clone()).collect();
        assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn next_tab_cycles_across_mixed_tabs() {
        let mut app = app_with_tabs(vec![claude_tab("c"), md_tab("m")]);
        app.next_tab();
        assert_eq!(app.active_session().unwrap().active_tab, 1);
        app.next_tab();
        assert_eq!(app.active_session().unwrap().active_tab, 0);
        app.prev_tab();
        assert_eq!(app.active_session().unwrap().active_tab, 1);
    }

    #[test]
    fn close_active_tab_removes_markdown() {
        let mut app = app_with_tabs(vec![claude_tab("c"), md_tab("m")]);
        app.sessions.get_mut("p").unwrap().active_tab = 1;
        app.close_active_tab();
        let s = app.active_session().unwrap();
        assert_eq!(s.tabs.len(), 1);
        assert!(matches!(s.tabs[0], Tab::Claude(_)));
    }

    #[test]
    fn status_update_routes_to_matching_tab_by_id() {
        use crate::ipc::StatusUpdate;
        use crate::status::{HookEvent, StatusKind};

        let mut app = app_with_tabs(vec![claude_tab("a"), md_tab("m"), claude_tab("b")]);
        // Give the two Claude tabs distinct ids (the markdown tab is skipped).
        {
            let s = app.sessions.get_mut("p").unwrap();
            for (tab, id) in s.tabs.iter_mut().zip(["ta", "", "tb"]) {
                if let Tab::Claude(c) = tab {
                    c.status_id = id.to_string();
                }
            }
        }

        app.handle_status_update(StatusUpdate {
            tab: "tb".into(),
            kind: StatusKind::Waiting,
        });
        app.handle_status_update(StatusUpdate {
            tab: "ta".into(),
            kind: StatusKind::SubagentStart,
        });
        // An unknown id must be a harmless no-op.
        app.handle_status_update(StatusUpdate {
            tab: "ghost".into(),
            kind: StatusKind::Busy,
        });

        let s = app.active_session().unwrap();
        let a = s.tabs[0].as_claude().unwrap();
        let b = s.tabs[2].as_claude().unwrap();
        assert_eq!(a.status.event, None);
        assert_eq!(a.status.subagents, 1);
        assert_eq!(b.status.event, Some(HookEvent::Waiting));
        assert_eq!(b.status.subagents, 0);
    }

    #[test]
    fn format_review_comments_markdown() {
        use crate::review::ReviewComment;

        assert_eq!(
            crate::format_review_comments(&[]),
            "No review comments were left.\n"
        );
        let md = crate::format_review_comments(&[ReviewComment {
            path: "src/main.rs".into(),
            side: "after".into(),
            line: 42,
            line_text: "    x.unwrap()".into(),
            body: "this can panic".into(),
        }]);
        assert!(md.starts_with("## Review comments (1)"));
        assert!(md.contains("- `src/main.rs:42` (after): this can panic"));
        assert!(md.contains("> x.unwrap()"));
    }

    #[test]
    fn close_active_tab_keeps_last_claude() {
        // Closing the only Claude tab is a no-op (a project keeps ≥1 session).
        let mut app = app_with_tabs(vec![claude_tab("c")]);
        app.close_active_tab();
        assert_eq!(app.active_session().unwrap().tabs.len(), 1);
    }

    #[test]
    fn open_markdown_tab_appends_and_focuses() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("doc.md"), "# Hi\n\nbody").unwrap();
        let mut app = empty_app();
        let name = "p".to_string();
        app.store.projects.push(Project {
            name: name.clone(),
            path: dir.path().to_path_buf(),
            tags: vec![],
            layout_mode: None,
            shell_passthrough: None,
            claude_sessions: vec![],
        });
        app.active_project_name = Some(name.clone());
        let session = ProjectSession {
            tabs: vec![claude_tab("claude")],
            ..Default::default()
        };
        app.sessions.insert(name.clone(), session);

        app.open_markdown_tab("doc.md").unwrap();
        let s = app.active_session().unwrap();
        assert_eq!(s.tabs.len(), 2);
        assert!(matches!(s.current(), Some(Tab::Markdown(_))));
        assert_eq!(app.focus, Focus::Claude);

        // Missing files surface an error rather than opening a tab.
        assert!(app.open_markdown_tab("does-not-exist.md").is_err());
    }

    #[test]
    fn add_markdown_tab_into_background_project_keeps_focus() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("d.md"), "# x").unwrap();
        let abs = dir.path().join("d.md").canonicalize().unwrap();
        let mut app = empty_app();
        app.sessions.insert(
            "bg".to_string(),
            ProjectSession {
                tabs: vec![claude_tab("c")],
                ..Default::default()
            },
        );
        app.active_project_name = Some("active".to_string());
        app.sessions.insert(
            "active".to_string(),
            ProjectSession {
                tabs: vec![claude_tab("c")],
                ..Default::default()
            },
        );
        app.focus = Focus::Shell;

        app.add_markdown_tab("bg", abs).unwrap();
        // The background project gained the tab, but focus is unchanged since it
        // isn't the active project.
        assert_eq!(app.sessions["bg"].tabs.len(), 2);
        assert_eq!(app.focus, Focus::Shell);

        // An unloaded project is an error.
        assert!(
            app.add_markdown_tab("nope", dir.path().join("d.md"))
                .is_err()
        );
    }

    #[test]
    fn unload_active_project_resets_state() {
        let mut app = empty_app();
        app.sessions
            .insert("foo".to_string(), ProjectSession::default());
        app.active_project_name = Some("foo".to_string());
        app.sidebar.active = Some("foo".to_string());
        app.focus = Focus::Shell;

        assert!(app.unload_project("foo"));
        assert!(!app.sessions.contains_key("foo"));
        assert_eq!(app.active_project_name, None);
        assert_eq!(app.sidebar.active, None);
        assert_eq!(app.focus, Focus::Projects);
    }

    #[test]
    fn unload_missing_project_is_noop() {
        let mut app = empty_app();
        // Never loaded → nothing to unload.
        assert!(!app.unload_project("nope"));
        assert_eq!(app.focus, Focus::Projects);
    }

    #[test]
    fn unload_background_project_keeps_active() {
        let mut app = empty_app();
        app.sessions
            .insert("bg".to_string(), ProjectSession::default());
        app.sessions
            .insert("active".to_string(), ProjectSession::default());
        app.active_project_name = Some("active".to_string());
        app.sidebar.active = Some("active".to_string());
        app.focus = Focus::Claude;

        assert!(app.unload_project("bg"));
        assert!(!app.sessions.contains_key("bg"));
        // Unloading a background project leaves the active one untouched.
        assert_eq!(app.active_project_name.as_deref(), Some("active"));
        assert_eq!(app.sidebar.active.as_deref(), Some("active"));
        assert_eq!(app.focus, Focus::Claude);
    }

    #[test]
    fn crlf_collapses_to_single_cr() {
        assert_eq!(normalize_paste("a\r\nb"), "a\rb");
    }

    #[test]
    fn lone_lf_becomes_cr() {
        assert_eq!(normalize_paste("a\nb\nc"), "a\rb\rc");
    }

    #[test]
    fn lone_cr_passes_through() {
        assert_eq!(normalize_paste("a\rb"), "a\rb");
    }

    #[test]
    fn strips_embedded_end_marker() {
        // A hostile paste embeds the bracketed-paste end marker to escape
        // the wrapping and inject control bytes; we must strip it.
        let s = "hello\x1b[201~rm -rf /";
        assert_eq!(normalize_paste(s), "hellorm -rf /");
    }

    #[test]
    fn preserves_other_escape_sequences() {
        // We only strip \e[201~; everything else (color codes, etc.) is
        // forwarded verbatim so the inner program can decide.
        let s = "\x1b[31mred\x1b[0m";
        assert_eq!(normalize_paste(s), s);
    }

    #[test]
    fn empty_in_empty_out() {
        assert_eq!(normalize_paste(""), "");
    }
}
