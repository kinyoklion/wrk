mod api;
mod ipc;
mod keymap;
mod notifications;
mod pane;
mod plugin;
mod proc;
mod remote;
mod review;
mod session;
mod settings;
mod status;
mod store;
mod ui;
mod widget;

use std::collections::{BTreeMap, HashMap};
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
use crate::settings::{HarnessKind, Settings, Theme};
use crate::store::{LayoutMode, Project, ProjectStore, SessionRef};
use crate::ui::ModalState;
use crate::ui::form::PluginFormModal;
use crate::ui::modal::{
    AddField, AddProjectModal, ClaudeTabPickerModal, ConfirmDeleteModal, ConfirmQuitModal,
    ConfirmUnloadModal, OpenMarkdownModal, UrlPickerModal,
};
use crate::ui::projects::{RowKey, SidebarGroup};

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
    /// Push a status update to the running wrk instance. Invoked by the agent
    /// hooks installed by `install-hooks`; reads `WRK_SOCK`/`WRK_TAB` from the
    /// environment and is a silent no-op outside a wrk session. `--harness kimi`
    /// additionally parses the session id off the hook's stdin JSON.
    #[command(hide = true)]
    Hook {
        kind: String,
        #[arg(long)]
        harness: Option<String>,
    },
    /// Start or end an in-TUI code review in the running wrk instance:
    /// `wrk review start [target]` / `wrk review end`. `target` is a git rev or
    /// `a..b` range; empty compares the working tree to HEAD.
    Review {
        #[command(subcommand)]
        action: ReviewAction,
    },
    /// Ask the user a yes/no question in the running wrk instance (from inside
    /// one of its panes) and wait for the answer. Exits 0 when they accept, 1
    /// when they decline, and 2 when there is no wrk to ask.
    Confirm {
        /// The question, one line.
        title: String,
        /// Detail shown under the question, a line each.
        lines: Vec<String>,
        /// What answering yes does, e.g. `download` (shown as `y: download`).
        #[arg(long)]
        accept: Option<String>,
        /// What answering no does (shown as `n / Esc: …`).
        #[arg(long)]
        decline: Option<String>,
    },
    /// Show a message in the running wrk instance's status bar (from inside one
    /// of its panes). A silent no-op outside a wrk session.
    Notify {
        message: String,
        /// Show it as an error.
        #[arg(long)]
        error: bool,
    },
    /// Drive the running wrk from a widget or script: list sessions, type into
    /// one, show one, post notifications, show/hide widgets. Runs inside wrk
    /// (needs `WRK_SOCK`). See docs/widgets.md.
    Api {
        #[command(subcommand)]
        action: api::ApiAction,
    },
}

/// `wrk review` subcommands. Modeled as real clap subcommands (rather than a
/// raw arg vec) so `wrk review start --help` prints help and a `target`
/// beginning with `-` is rejected instead of opening a review against a bogus
/// rev (#95).
#[derive(Subcommand)]
enum ReviewAction {
    /// Open the review overlay for the git repository enclosing the current
    /// directory. `target` is a git rev or `a..b` range; omit it to compare the
    /// working tree to HEAD.
    Start {
        /// Git revision or `a..b` range to review (default: working tree vs HEAD).
        target: Option<String>,
    },
    /// Close the active review overlay and print the collected comments.
    End,
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
        Some(Command::Hook { kind, harness }) => cmd_hook(&kind, harness.as_deref()),
        Some(Command::Review { action }) => cmd_review(action),
        Some(Command::Confirm {
            title,
            lines,
            accept,
            decline,
        }) => cmd_confirm(title, lines, accept, decline),
        Some(Command::Notify { message, error }) => cmd_notify(message, error),
        Some(Command::Api { action }) => api::cmd_api(action),
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

/// `wrk confirm`: ask the running instance (`WRK_SOCK`) and exit with the
/// answer: 0 accepted, 1 declined, 2 no wrk to ask (or it exited unanswered).
fn cmd_confirm(
    title: String,
    lines: Vec<String>,
    accept: Option<String>,
    decline: Option<String>,
) -> Result<()> {
    let Some(sock) = std::env::var_os("WRK_SOCK").filter(|s| !s.is_empty()) else {
        eprintln!("wrk confirm: not inside a wrk pane (no WRK_SOCK)");
        std::process::exit(2);
    };
    let req = ipc::ConfirmRequest {
        project: std::env::var("WRK_PROJECT").ok().filter(|s| !s.is_empty()),
        title,
        lines,
        accept,
        decline,
    };
    match ipc::ask(Path::new(&sock), &req) {
        Ok(Some(true)) => std::process::exit(0),
        Ok(Some(false)) => std::process::exit(1),
        Ok(None) => {
            eprintln!("wrk confirm: wrk closed without an answer");
            std::process::exit(2);
        }
        Err(e) => {
            eprintln!("wrk confirm: {e:#}");
            std::process::exit(2);
        }
    }
}

/// `wrk notify`: a status-bar message for the running instance; nothing
/// outside a wrk session.
fn cmd_notify(message: String, error: bool) -> Result<()> {
    if let Some(sock) = std::env::var_os("WRK_SOCK").filter(|s| !s.is_empty()) {
        let req = ipc::Request::Notify(ipc::NotifyRequest {
            project: std::env::var("WRK_PROJECT").ok().filter(|s| !s.is_empty()),
            message,
            error,
        });
        let _ = ipc::send(Path::new(&sock), &req);
    }
    Ok(())
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
    // Install hooks + skills for every enabled harness (Claude by default; Kimi
    // when opted in via `[harness.kimi] enabled = true`). Settings that fail to
    // load fall back to defaults so `install-hooks` still works out of the box.
    let settings = settings::load().unwrap_or_default();
    for harness in settings.enabled_harnesses() {
        match harness {
            HarnessKind::Claude => {
                let path = status::install_hooks()?;
                println!("installed claude hooks in {}", path.display());
                match status::install_skills() {
                    Ok(skills) => {
                        for path in skills {
                            println!("installed claude skill {}", path.display());
                        }
                    }
                    Err(e) => eprintln!("warning: could not install claude skills: {e}"),
                }
            }
            HarnessKind::Kimi => {
                let path = status::install_kimi_hooks()?;
                println!("installed kimi hooks in {}", path.display());
                match status::install_kimi_skills() {
                    Ok(skills) => {
                        for path in skills {
                            println!("installed kimi skill {}", path.display());
                        }
                    }
                    Err(e) => eprintln!("warning: could not install kimi skills: {e}"),
                }
            }
        }
    }
    println!("hooks push status to the running wrk over its socket ($WRK_SOCK)");
    println!("(no-op for any agent session not launched by wrk)");
    Ok(())
}

/// Push a status update to the running wrk instance. Invoked by agent hooks as
/// `wrk hook <kind> [--harness <name>]`; reads `WRK_SOCK` (the instance socket)
/// and `WRK_TAB` (the originating tab id) from the environment. Best-effort and
/// always exits 0 — a missing socket, stale instance, or unknown kind is a
/// silent no-op so hooks never surface errors to the agent.
///
/// The hook's stdin carries the agent's event JSON; its `session_id` and
/// `source` are forwarded so wrk can follow a tab whose session changed (Claude
/// `/clear`, `/resume`) and learn a Kimi tab's id. Missing/garbled stdin simply
/// omits them. `--harness` is still accepted (installed Kimi hooks pass it) but
/// no longer changes behavior.
fn cmd_hook(kind: &str, _harness: Option<&str>) -> Result<()> {
    let (Ok(sock), Ok(tab)) = (std::env::var("WRK_SOCK"), std::env::var("WRK_TAB")) else {
        return Ok(());
    };
    let Some(kind) = status::StatusKind::from_arg(kind) else {
        return Ok(());
    };
    let (session_id, source) = read_stdin_hook_event();
    let req = ipc::Request::Status(ipc::StatusUpdate {
        tab,
        kind,
        session_id,
        source,
    });
    let _ = ipc::send(Path::new(&sock), &req);
    Ok(())
}

/// Read the hook event JSON on stdin and pull out `session_id` and `source`.
/// Best-effort: any read/parse failure (or an absent field) yields `None`. The
/// agent writes the JSON and closes stdin, so the read ends promptly at EOF; an
/// interactive stdin (a manual `wrk hook` run) is skipped rather than blocking.
fn read_stdin_hook_event() -> (Option<String>, Option<String>) {
    use std::io::{IsTerminal, Read};
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return (None, None);
    }
    let mut buf = String::new();
    if stdin.lock().read_to_string(&mut buf).is_err() {
        return (None, None);
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(buf.trim()) else {
        return (None, None);
    };
    let field = |k: &str| {
        value
            .get(k)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    (field("session_id"), field("source"))
}

/// Start or end an in-TUI code review in the running wrk instance:
/// `wrk review start [target]` opens the review overlay; `wrk review end`
/// closes it. Both require being inside a wrk-managed pane (`WRK_SOCK` set).
fn cmd_review(action: ReviewAction) -> Result<()> {
    let sock = std::env::var("WRK_SOCK").ok().filter(|s| !s.is_empty());
    match action {
        ReviewAction::Start { target } => {
            let target = target.unwrap_or_default();
            let target = target.trim();
            // Refuse a bad target (e.g. one starting with `-`, which would
            // reach git as an option) before touching the running wrk (#95).
            let parsed = review::diff::DiffTarget::parse(target)?;
            let sock = sock.ok_or_else(|| {
                anyhow!("`wrk review` must be run inside a wrk-managed pane (no WRK_SOCK)")
            })?;
            // Send the directory `wrk review start` ran in so the TUI diffs the
            // git repo enclosing it — the project dir may be a non-git container
            // of clones/worktrees, so its own cwd can't be assumed (#94).
            let cwd = std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned());
            let req = ipc::Request::Review(ipc::ReviewRequest {
                tab: std::env::var("WRK_TAB").ok(),
                project: std::env::var("WRK_PROJECT").ok(),
                cwd,
                kind: ipc::ReviewKind::Start {
                    target: if target.is_empty() {
                        None
                    } else {
                        Some(target.to_string())
                    },
                },
            });
            ipc::send(Path::new(&sock), &req).context("sending review request to wrk")?;
            println!(
                "Opened a code review ({}) in the wrk review pane. \
                 Review it there, then run /end-local-review to collect comments.",
                parsed.label()
            );
            Ok(())
        }
        ReviewAction::End => {
            // Read the mirrored comments (written by the running TUI) and print
            // them as markdown for Claude to ingest, then ask the TUI to close.
            let key = review_env_key();
            let comments = review::read_mirror(&key);
            print!("{}", format_review_comments(&comments));
            if let Some(sock) = sock {
                let req = ipc::Request::Review(ipc::ReviewRequest {
                    tab: std::env::var("WRK_TAB").ok(),
                    project: std::env::var("WRK_PROJECT").ok(),
                    cwd: None,
                    kind: ipc::ReviewKind::End,
                });
                let _ = ipc::send(Path::new(&sock), &req);
            }
            Ok(())
        }
    }
}

/// Mirror-file key for an active review, prefixed with this instance's pid so
/// concurrent wrk instances never share a mirror file and a new instance never
/// reads a dead predecessor's leftover mirror (a per-process `WRK_TAB` like
/// "tab0" is otherwise reused by every instance). The pid also lets a startup
/// sweep reclaim dead instances' mirrors. Suffix: the tab id, else the project.
fn review_mirror_key(session: &review::ReviewSession) -> String {
    let suffix = session
        .tab_id
        .clone()
        .unwrap_or_else(|| session.project.clone());
    format!("{}-{suffix}", std::process::id())
}

/// The mirror key from the environment (client side of `wrk review end`). Must
/// match [`review_mirror_key`]: the owning instance's pid (parsed from the
/// `WRK_SOCK` path, which is `…/wrk-<pid>.sock`) + `WRK_TAB` (else `WRK_PROJECT`).
fn review_env_key() -> String {
    let suffix = std::env::var("WRK_TAB")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("WRK_PROJECT").ok())
        .unwrap_or_default();
    match std::env::var("WRK_SOCK")
        .ok()
        .and_then(|s| ipc::pid_from_socket(&s))
    {
        Some(pid) => format!("{pid}-{suffix}"),
        None => suffix,
    }
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
    // Remove wrk's hooks/skills from every harness regardless of enabled state,
    // so disabling a harness first doesn't strand its entries.
    let (path, removed) = status::uninstall_hooks()?;
    println!("removed {removed} wrk hook entries from {}", path.display());
    match status::uninstall_skills() {
        Ok(dirs) => {
            for dir in dirs {
                println!("removed claude skill {}", dir.display());
            }
        }
        Err(e) => eprintln!("warning: could not remove claude skills: {e}"),
    }
    match status::uninstall_kimi_hooks() {
        Ok((path, removed)) => {
            println!("removed {removed} wrk hook entries from {}", path.display())
        }
        Err(e) => eprintln!("warning: could not remove kimi hooks: {e}"),
    }
    match status::uninstall_kimi_skills() {
        Ok(dirs) => {
            for dir in dirs {
                println!("removed kimi skill {}", dir.display());
            }
        }
        Err(e) => eprintln!("warning: could not remove kimi skills: {e}"),
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
        session_names: BTreeMap::new(),
        plugin: None,
        plugin_config: BTreeMap::new(),
        remote: None,
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

/// A single running (or dead) agent tab within a project. Named `ClaudeTab` for
/// historical reasons; it now hosts any [`HarnessKind`] (Claude, Kimi, …).
pub struct ClaudeTab {
    pub name: String,
    /// Which coding-agent harness this tab runs.
    pub harness: HarnessKind,
    /// Agent session id — used to resume the same conversation. For Claude, wrk
    /// mints a fresh UUID at first spawn (`--session-id <uuid>`) and records it
    /// here. For Kimi it's the id the agent reveals via its `SessionStart`/status
    /// hooks (recorded when it arrives); Kimi 2.0.0 does not fire config hooks, so
    /// in practice this stays `None` and Kimi resumes via `--continue` instead
    /// (see [`ClaudeTab::fresh`]). A later open resumes from this id when present.
    pub session_id: Option<String>,
    /// Transient (not persisted): true only for a just-created tab that should
    /// start a *new* session. Distinguishes "add a new Kimi tab" (fresh `kimi`)
    /// from "reopen an existing one" (`kimi --continue`), since without a learned
    /// session id those look identical. Cleared after the first spawn. Ignored by
    /// Claude (its `session_id` drives new-vs-resume).
    pub fresh: bool,
    /// Transient: whether a hook has reported this tab's session id since its
    /// last spawn. The first report is the launch itself — a differing id there
    /// is the same conversation under a new id, not a user-initiated switch.
    pub id_reported: bool,
    /// Opaque per-tab id exported to the PTY as `WRK_TAB`, so hook-driven status
    /// pushes (`wrk hook`) can be routed back to this tab. Stable for the tab's
    /// lifetime, independent of the Claude session ID.
    pub status_id: String,
    /// Latest hook-driven status (state + sub-agent count). Defaults to "no
    /// event yet", at which point the UI falls back to the idle-time heuristic.
    pub status: status::TabStatus,
    pub pane: Option<PtyPane>,
    /// Transient: where this tab's plugin attach stands, for a plugin-backed
    /// project (always `Idle` for an internal one). Drives the pane placeholder
    /// while the command is resolved off the UI thread, and stops a second
    /// attach being queued while one is in flight.
    pub plugin_attach: PluginAttach,
    /// Transient: attach this tab when it is first shown, rather than when its
    /// project opens. Set on a plugin-listed project's tabs (a box may hold
    /// many sessions; only those looked at get a pane), cleared once asked.
    pub attach_when_shown: bool,
}

/// Progress of a plugin tab's attach (see [`ClaudeTab::plugin_attach`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PluginAttach {
    /// Nothing in flight: an internal tab, or a plugin tab that is running or
    /// whose session has ended.
    #[default]
    Idle,
    /// An attach job is in flight; the pane appears when it completes.
    Pending,
    /// The last attach failed; the reason is shown in the pane.
    Failed(String),
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
    /// Transient: a plugin shell attach is in flight (plugin projects only).
    pub shell_pending: bool,
    /// The session the shell was last asked for. A plugin-listed project's
    /// shell follows the shown tab, so a different shown session re-asks.
    pub shell_for: Option<String>,
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
    /// Session plugins: runs attach/list/close jobs off the UI thread and
    /// hands back results, drained each tick by the event loop.
    pub plugins: plugin::PluginHost,
    /// The registered plugins' names (settings are fixed for the run).
    pub plugin_names: Vec<String>,
    /// Projects plugins list themselves, each plugin's a sidebar group.
    pub remote: remote::Remote,
    /// Widget programs (`[widget.<name>]`): the running ones and the one on
    /// screen, if any.
    pub widgets: widget::Widgets,
    /// `[widget.<name>] key` bindings, each summoning its widget.
    pub widget_keys: HashMap<KeyEvent, String>,
    /// Notification banners posted through the API.
    pub notifications: notifications::Notifications,
    /// `wrk api send --submit` Enters waiting out their pause: when, and the
    /// session id to press it in.
    pub pending_submits: Vec<(Instant, String)>,
    /// `wrk confirm` prompts waiting for the one on screen to be answered.
    pub prompts: std::collections::VecDeque<ui::prompt::PromptModal>,
    /// `wrk api create` requests waiting on their plugin, by ticket.
    pub api_creates: Vec<api::PendingCreate>,
    pub next_create_ticket: u64,
}

impl App {
    fn new(store: ProjectStore, settings: Settings) -> Self {
        let sidebar = ui::projects::ProjectSidebar::default();
        let theme = settings.theme.resolve();
        let md_theme = settings.markdown.resolve();
        let heading_images = settings.markdown.heading_images.unwrap_or(true);
        let plugins = plugin::PluginHost::new(settings.plugin.clone(), plugin::default_base_dir());
        let (keymap, keymap_warnings) = KeyMap::build(&settings.keys.global);
        // Surface any keymap warnings (invalid bindings, conflicts) via the
        // status-bar error slot; they remain visible until the user dismisses
        // them by triggering any other action that overwrites `app.error`.
        let mut error = None;
        for w in keymap_warnings {
            push_error(&mut error, w);
        }
        let (widget_keys, widget_key_warnings) = widget::summon_keys(&settings.widget, &keymap);
        for w in widget_key_warnings {
            push_error(&mut error, w);
        }
        let mut app = Self {
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
            layout_mode: LayoutMode::Tabbed,
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
            plugins,
            plugin_names: Vec::new(),
            remote: remote::Remote::default(),
            widgets: widget::Widgets::default(),
            widget_keys,
            notifications: notifications::Notifications::default(),
            pending_submits: Vec::new(),
            prompts: std::collections::VecDeque::new(),
            api_creates: Vec::new(),
            next_create_ticket: 1,
        };
        app.plugin_names = app.settings.plugin_names();
        app.refresh_sidebar();
        app
    }

    /// Rebuild the sidebar's project/session tree from the store and the live
    /// sessions. The event loop runs this before every draw (so statuses and tab
    /// lists stay current); handlers that change what it lists call it so their
    /// follow-up input sees the new rows. While focus is elsewhere the cursor
    /// follows the session on screen, so jumping to the sidebar lands on it.
    pub fn refresh_sidebar(&mut self) {
        let active = self.active_project_name.as_deref();
        let mut groups = vec![SidebarGroup {
            id: SidebarGroup::LOCAL.into(),
            // The local heading only shows beside plugin groups.
            label: (!self.remote.groups.is_empty()).then(|| "local".into()),
            note: None,
            projects: self.store.projects.iter().collect(),
        }];
        for group in &self.remote.groups {
            let note = if group.error.is_some() {
                Some("⚠ unavailable".to_string())
            } else if !group.listed {
                Some("loading…".to_string())
            } else if group.projects.is_empty() {
                Some("none yet: +".to_string())
            } else {
                None
            };
            groups.push(SidebarGroup {
                id: group.plugin.clone(),
                label: Some(group.label.clone()),
                note,
                projects: group.projects.iter().collect(),
            });
        }
        self.sidebar.rebuild(&groups, |p| {
            ui::project_view(&self.sessions, p, active == Some(p.name.as_str()))
        });
        if self.focus != Focus::Projects
            && let Some(name) = active
            && let Some(session) = self.sessions.get(name)
        {
            self.sidebar
                .select_key(&RowKey::Session(name.to_string(), session.active_tab));
        }
    }

    /// Whether the primary pane draws its tab strip for a project with
    /// `tab_count` tabs. The sidebar lists every session nested under its
    /// project, so the strip only appears while the sidebar is hidden.
    pub fn tab_strip_shown(&self, tab_count: usize) -> bool {
        self.sidebar_hidden && tab_count > 0
    }

    /// [`claude_pane_split`] of the primary pane's `inner` rect for the active
    /// project: the tab strip (when shown) and the content area below it.
    pub fn active_primary_split(&self, inner: Rect) -> (Option<Rect>, Rect) {
        let count = self.active_session().map(|s| s.tabs.len()).unwrap_or(0);
        claude_pane_split(inner, self.tab_strip_shown(count))
    }

    pub fn active_project(&self) -> Option<&Project> {
        self.project(self.active_project_name.as_ref()?)
    }

    /// The project keyed `name`: one from projects.toml, or one a plugin listed.
    pub fn project(&self, name: &str) -> Option<&Project> {
        self.store.find(name).or_else(|| self.remote.find(name))
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
    /// (so `wrk view` can name its originating project), `WRK_SOCK` (this
    /// instance's IPC socket), and `WRK_BIN` (the path to *this* running `wrk`
    /// binary). Hooks and skills invoke `"$WRK_BIN" …` rather than hardcoding a
    /// path, so nothing machine-specific is baked into `~/.claude` — it works
    /// wherever this instance runs, including under `cargo run`. Claude tabs
    /// additionally get `WRK_TAB`.
    fn base_pty_env(&self, project_name: &str) -> Vec<(String, String)> {
        let mut env = vec![("WRK_PROJECT".to_string(), project_name.to_string())];
        if let Some(sock) = &self.socket_path {
            env.push(("WRK_SOCK".to_string(), sock.to_string_lossy().into_owned()));
        }
        if let Ok(exe) = std::env::current_exe() {
            env.push(("WRK_BIN".to_string(), exe.to_string_lossy().into_owned()));
        }
        env
    }

    /// Open the sidebar's selection: its project (spawning whatever isn't
    /// running) and, for a session row, that tab.
    fn open_selected(&mut self, body: Rect) {
        let Some((name, tab)) = self.sidebar.selected_target() else {
            // A plugin group's heading: what it offers is creating something.
            if let Some(group) = self.sidebar.selected_group().map(str::to_string)
                && group != SidebarGroup::LOCAL
            {
                self.open_plugin_form(&group, None);
            }
            return;
        };
        self.open_project(&name, tab, body);
    }

    /// Open project `name` (spawning whatever isn't running) and, when `tab` is
    /// given, show that tab.
    fn open_project(&mut self, name: &str, tab: Option<usize>, body: Rect) {
        let Some(project) = self.project(name).cloned() else {
            return;
        };
        self.active_project_name = Some(project.name.clone());
        self.sidebar.active = Some(project.name.clone());
        self.layout_mode = project.layout_mode.unwrap_or_default();
        self.shell_passthrough = project.shell_passthrough.unwrap_or(false);
        self.error = None;

        let layout = compute_layout(body, self);
        let claude_inner = layout.claude_inner();
        let shell_inner = layout.shell_inner();
        let base_env = self.base_pty_env(&project.name);
        let sidebar_hidden = self.sidebar_hidden;

        let session = self.sessions.entry(project.name.clone()).or_default();

        // Build the initial set of tabs from the project's configured sessions.
        // If the list is empty, create one new session named after the project.
        // A tab with no `session_id` (empty project, or a hand-written entry)
        // gets a fresh UUID assigned at spawn time and is persisted, so every
        // later open resumes deterministically via `--resume <id>`.
        if session.tabs.is_empty() {
            if project.claude_sessions.is_empty() && project.is_remote() {
                // A listed project shows only the sessions its plugin lists;
                // an empty one waits for Alt+n.
            } else if project.claude_sessions.is_empty() {
                // Empty project: one auto-tab. Not marked `fresh` — a Kimi auto-tab
                // `--continue`s (resumes the dir's latest session, or starts fresh
                // if none); Claude still mints a new session at spawn.
                session.tabs.push(Tab::Claude(ClaudeTab {
                    name: project.name.clone(),
                    harness: self.settings.default_harness(),
                    session_id: None,
                    fresh: false,
                    id_reported: false,
                    status_id: new_status_id(),
                    status: status::TabStatus::default(),
                    pane: None,
                    plugin_attach: PluginAttach::Idle,
                    attach_when_shown: false,
                }));
            } else {
                // Reopening persisted tabs — resume, never restart.
                for sr in &project.claude_sessions {
                    session.tabs.push(Tab::Claude(ClaudeTab {
                        name: sr.name.clone(),
                        harness: sr.harness,
                        session_id: sr.session_id.clone(),
                        fresh: false,
                        id_reported: false,
                        status_id: new_status_id(),
                        status: status::TabStatus::default(),
                        pane: None,
                        plugin_attach: PluginAttach::Idle,
                        attach_when_shown: project.is_remote(),
                    }));
                }
            }
        }

        // `self.tab_strip_shown`, inlined: `session` mutably borrows `self`.
        let strip = sidebar_hidden && !session.tabs.is_empty();
        let claude_content = claude_pane_split(claude_inner, strip).1;
        let mut assigned_new = false;
        // An internal project spawns its harness PTYs right here. A plugin
        // project's commands come from its plugin, so its attaches are queued
        // (below) and the panes appear as results arrive — the UI never waits
        // on a plugin.
        if project.is_internal() {
            // Spawn any missing or dead claude panes. Tabs without a session_id are
            // assigned a fresh UUID here (`--session-id`) and persisted afterward.
            for tab in session.claude_tabs_mut() {
                let dead = tab.pane.as_mut().is_some_and(|p| p.child_finished());
                if tab.pane.is_none() || dead {
                    let cmd = spawn_command_for_tab(&self.settings, tab, &mut assigned_new);
                    tab.id_reported = false;
                    let mut env = base_env.clone();
                    env.push(("WRK_TAB".to_string(), tab.status_id.clone()));
                    env.extend(self.settings.harness_pane_env(tab.harness));
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

        if let Some(tab) = tab
            && let Some(session) = self.sessions.get_mut(&project.name)
            && tab < session.tabs.len()
        {
            session.active_tab = tab;
        }

        if project.is_remote() {
            self.request_shown_remote_pane(&project);
        } else if let Some(plugin) = &project.plugin {
            self.request_plugin_panes(&project, plugin);
        }
        self.focus = Focus::Claude;
    }

    /// Opening a listed project attaches only the tab on screen (re-attaching
    /// it after a failure or a detach), and lets the shell be asked again; the
    /// rest attach when shown ([`App::ensure_remote_panes`]).
    fn request_shown_remote_pane(&mut self, project: &Project) {
        let Some(plugin) = project.plugin.clone() else {
            return;
        };
        let payload = plugin::payload(project);
        let Some(session) = self.sessions.get_mut(&project.name) else {
            return;
        };
        if session.shell.is_none() && !session.shell_pending {
            session.shell_for = None;
        }
        let Some(tab) = session.active_claude_tab_mut() else {
            return;
        };
        if tab.pane.as_mut().is_some_and(|p| p.child_finished()) {
            tab.pane = None;
        }
        if tab.pane.is_none() && tab.plugin_attach != PluginAttach::Pending {
            tab.plugin_attach = PluginAttach::Pending;
            tab.attach_when_shown = false;
            let job = plugin::Job::Attach {
                project: payload,
                tab: tab.status_id.clone(),
                session_id: tab.session_id.clone(),
                fresh: false,
                name: tab.name.clone(),
            };
            self.plugins.submit(&plugin, job);
        }
    }

    /// Each tick, for a listed project on screen: attach the shown tab the
    /// first time it is shown, and point the shell at the shown tab's session
    /// (while the shell is visible). Plugin jobs run off the UI thread.
    fn ensure_remote_panes(&mut self) {
        let Some(name) = self.active_project_name.as_deref() else {
            return;
        };
        let Some(project) = self.remote.find(name) else {
            return;
        };
        let Some(plugin) = project.plugin.clone() else {
            return;
        };
        let payload = plugin::payload(project);
        let shell_shown = self.layout_mode == LayoutMode::Split || self.focus == Focus::Shell;
        let Some(session) = self.sessions.get_mut(name) else {
            return;
        };
        let mut jobs = Vec::new();
        if let Some(tab) = session.active_claude_tab_mut()
            && tab.attach_when_shown
            && tab.pane.is_none()
            && tab.plugin_attach == PluginAttach::Idle
        {
            tab.attach_when_shown = false;
            tab.plugin_attach = PluginAttach::Pending;
            jobs.push(plugin::Job::Attach {
                project: payload.clone(),
                tab: tab.status_id.clone(),
                session_id: tab.session_id.clone(),
                fresh: false,
                name: tab.name.clone(),
            });
        }
        let want = session
            .active_claude_tab()
            .and_then(|t| t.session_id.clone());
        if shell_shown && want.is_some() && want != session.shell_for && !session.shell_pending {
            session.shell = None;
            session.shell_pending = true;
            session.shell_for = want.clone();
            jobs.push(plugin::Job::Shell {
                project: payload,
                session_id: want,
            });
        }
        for job in jobs {
            self.plugins.submit(&plugin, job);
        }
    }

    /// Ask the plugins whose listing is due for their projects.
    fn poll_remote(&mut self) {
        for plugin in self.remote.due(&self.plugin_names, Instant::now()) {
            self.plugins.submit(&plugin, plugin::Job::ListProjects);
        }
    }

    /// Fold in a plugin's listing: its sidebar group, the tabs of its projects
    /// that are open, and any just-created project waiting to be opened.
    fn apply_listing(&mut self, plugin: &str, result: Result<Option<plugin::Listing>>, body: Rect) {
        let error_before = self.remote.group(plugin).and_then(|g| g.error.clone());
        let vanished = self.remote.apply(
            plugin,
            result.map_err(|e| format!("{e:#}")),
            &remote::home_dir(),
        );
        if let Some(error) = self.remote.group(plugin).and_then(|g| g.error.clone())
            && Some(&error) != error_before.as_ref()
        {
            push_error(&mut self.error, format!("plugin '{plugin}': {error}"));
        }
        for name in vanished {
            self.unload_project(&name);
        }
        if let Some(group) = self.remote.group(plugin) {
            for project in &group.projects {
                if let Some(session) = self.sessions.get_mut(&project.name) {
                    reconcile_remote_tabs(session, &project.claude_sessions);
                }
            }
        }
        if let Some((name, tab)) = self.remote.take_ready_open(Instant::now()) {
            let row = match tab {
                Some(tab) => RowKey::Session(name.clone(), tab),
                None => RowKey::Project(name.clone()),
            };
            self.refresh_sidebar();
            self.sidebar.select_key(&row);
            self.open_project(&name, tab, body);
        }
    }

    /// Open plugin `plugin`'s creation form (fetched off the UI thread), from
    /// listed project `context` when there is one, so it can be preselected.
    fn open_plugin_form(&mut self, plugin: &str, context: Option<String>) {
        let Some(group) = self.remote.group(plugin) else {
            return;
        };
        if !group.can_create {
            self.info = Some(format!("{} can't create projects or sessions", group.label));
            return;
        }
        let payload = context
            .as_deref()
            .and_then(|key| self.remote.find(key))
            .map(plugin::payload);
        self.modal = Some(ModalState::PluginForm(PluginFormModal::loading(
            plugin.to_string(),
            group.label.clone(),
            context,
        )));
        self.plugins
            .submit(plugin, plugin::Job::CreateForm { project: payload });
    }

    /// The plugin's form description arrived: fill the open form with it.
    fn show_plugin_form(
        &mut self,
        plugin: &str,
        purpose: plugin::FormFor,
        result: Result<plugin::FormResponse>,
    ) {
        let Some(ModalState::PluginForm(m)) = self.modal.as_mut() else {
            return;
        };
        if m.plugin != plugin || m.purpose != purpose || m.form.is_some() {
            return;
        }
        match result {
            Ok(form) => m.set_form(form),
            Err(e) => m.error = Some(format!("couldn't load the form: {e:#}")),
        }
    }

    /// A `create` finished: close its form (or say why it failed in it), list
    /// the plugin's projects again, and open what it made once it's listed.
    fn finish_plugin_create(&mut self, plugin: &str, result: Result<plugin::CreateResponse>) {
        let form = match self.modal.as_mut() {
            Some(ModalState::PluginForm(m))
                if m.plugin == plugin && m.submitting && m.purpose == plugin::FormFor::Create =>
            {
                Some(m)
            }
            _ => None,
        };
        match result {
            Err(e) => match form {
                Some(m) => {
                    m.submitting = false;
                    m.error = Some(format!("{e:#}"));
                }
                None => push_error(&mut self.error, format!("plugin '{plugin}': {e:#}")),
            },
            Ok(created) => {
                if form.is_some() {
                    self.modal = None;
                }
                self.info = Some(
                    created
                        .message
                        .clone()
                        .unwrap_or_else(|| "created; it opens once it's listed".into()),
                );
                if let Some(project) = created.project {
                    self.remote.open_when_listed(
                        remote::key(plugin, &project),
                        created.session,
                        Instant::now(),
                    );
                }
                self.remote.refresh_soon(plugin);
            }
        }
    }

    /// The session of tab `tab` of listed project `key`, as the sidebar
    /// shows it: an open tab's, or a listed one's before the project opens.
    /// `None` for a markdown tab.
    fn listed_session_at(&self, key: &str, tab: usize) -> Option<String> {
        match self.sessions.get(key) {
            Some(session) => match session.tabs.get(tab)? {
                Tab::Claude(c) => c.session_id.clone(),
                Tab::Markdown(_) => None,
            },
            None => self
                .remote
                .find(key)?
                .claude_sessions
                .get(tab)?
                .session_id
                .clone(),
        }
    }

    /// `d` on a listed project, or on tab `tab` of one: its plugin's removal
    /// form, when it removes things.
    fn remove_listed(&mut self, key: &str, tab: Option<usize>) {
        let Some(project) = self.remote.find(key) else {
            return;
        };
        let Some(r) = project.remote.as_ref() else {
            return;
        };
        let can_remove = self.remote.group(&r.plugin).is_some_and(|g| g.can_remove);
        if !can_remove {
            self.info = Some(format!(
                "'{}' is listed by plugin '{}'; remove it there",
                r.label, r.plugin
            ));
            return;
        }
        let session_id = match tab {
            None => None,
            Some(tab) => match self.listed_session_at(key, tab) {
                Some(id) => Some(id),
                None => {
                    self.info = Some("a document tab: Alt+w closes it".into());
                    return;
                }
            },
        };
        let plugin = r.plugin.clone();
        let heading = self
            .remote
            .group(&plugin)
            .map_or_else(|| plugin.clone(), |g| g.label.clone());
        let payload = plugin::payload(project);
        self.modal = Some(ModalState::PluginForm(PluginFormModal::removing(
            plugin.clone(),
            heading,
            key.to_string(),
            session_id.clone(),
        )));
        self.plugins.submit(
            &plugin,
            plugin::Job::RemoveForm {
                project: payload,
                session_id,
            },
        );
    }

    /// A `remove` finished: close its form (or say why it failed in it), and
    /// list the plugin's projects again, which drops what's gone.
    fn finish_plugin_remove(&mut self, plugin: &str, result: Result<plugin::RemoveResponse>) {
        let form = match self.modal.as_mut() {
            Some(ModalState::PluginForm(m))
                if m.plugin == plugin && m.submitting && m.purpose == plugin::FormFor::Remove =>
            {
                Some(m)
            }
            _ => None,
        };
        match result {
            Err(e) => match form {
                Some(m) => {
                    m.submitting = false;
                    m.error = Some(format!("{e:#}"));
                }
                None => push_error(&mut self.error, format!("plugin '{plugin}': {e:#}")),
            },
            Ok(removed) => {
                if form.is_some() {
                    self.modal = None;
                }
                self.info = Some(removed.message.unwrap_or_else(|| {
                    "removed; it leaves the sidebar once it's no longer listed".into()
                }));
                self.remote.refresh_soon(plugin);
            }
        }
    }

    fn reload_store(&mut self) -> Result<()> {
        self.store = store::load()?;

        // Drop sessions whose project is no longer in the store.
        let known: std::collections::HashSet<&str> = self
            .store
            .projects
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        let remote = &self.remote;
        self.sessions
            .retain(|name, _| known.contains(name.as_str()) || remote.contains(name));

        if let Some(name) = &self.active_project_name
            && !known.contains(name.as_str())
            && !self.remote.contains(name)
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
        self.refresh_sidebar();
        Ok(())
    }

    /// Add a new agent tab (running `harness`) to the active project.
    /// `session_id = Some(id)` resumes that session. `session_id = None` starts a
    /// new one: Claude mints a fresh UUID up front and records it immediately;
    /// Kimi starts a bare new session (the tab is persisted so a later reopen
    /// resumes it via `--continue`).
    fn add_claude_tab(
        &mut self,
        name: String,
        harness: HarnessKind,
        session_id: Option<String>,
        body: Rect,
    ) {
        let Some(project_name) = self.active_project_name.clone() else {
            return;
        };
        // A plugin project's sessions come from its plugin, off the UI thread.
        if let Some(project) = self.active_project().filter(|p| !p.is_internal()).cloned() {
            self.add_plugin_tab(&project, name, session_id);
            return;
        }
        let layout = compute_layout(body, self);
        let claude_inner = layout.claude_inner();
        // After this spawn there will be at least one tab → strip will render →
        // size the PTY to the post-strip content height.
        let existing = self
            .sessions
            .get(&project_name)
            .map(|s| s.tabs.len())
            .unwrap_or(0);
        let claude_content = claude_pane_split(claude_inner, self.tab_strip_shown(existing + 1)).1;

        let Some(project_path) = self.active_project().map(|p| p.path.clone()) else {
            return;
        };

        // Resume an existing session, or start a new one. Claude mints its UUID
        // here so it's tracked from the first spawn; Kimi keeps `session_id` None
        // until its SessionStart hook reveals the id (handle_status_update).
        let (cmd, session_id) = match (session_id, harness) {
            (Some(id), h) => (resume_agent_command(&self.settings, h, &id), Some(id)),
            (None, HarnessKind::Claude) => {
                let id = new_session_id();
                let cmd = new_agent_command(&self.settings, HarnessKind::Claude, &id, &name);
                (cmd, Some(id))
            }
            (None, h) => (new_agent_command(&self.settings, h, "", &name), None),
        };
        let status_id = new_status_id();
        let mut env = self.base_pty_env(&project_name);
        env.push(("WRK_TAB".to_string(), status_id.clone()));
        env.extend(self.settings.harness_pane_env(harness));

        let pane = match PtyPane::spawn(
            &cmd,
            &project_path,
            claude_content.height,
            claude_content.width,
            &env,
        ) {
            Ok(p) => Some(p),
            Err(e) => {
                push_error(
                    &mut self.error,
                    format!("{} spawn failed: {e}", harness.id()),
                );
                None
            }
        };

        let session = self.sessions.entry(project_name.clone()).or_default();
        session.tabs.push(Tab::Claude(ClaudeTab {
            name,
            harness,
            session_id,
            // The initial (fresh) spawn happened above; a later respawn of this
            // Kimi tab should resume via `--continue`, not restart.
            fresh: false,
            id_reported: false,
            status_id,
            status: status::TabStatus::default(),
            pane,
            plugin_attach: PluginAttach::Idle,
            attach_when_shown: false,
        }));
        let new_idx = session.tabs.len() - 1;
        session.active_tab = new_idx;

        // Persist to store.
        self.persist_claude_sessions(&project_name);
    }

    /// Add a tab to plugin project `project`. It appears at once (as
    /// "connecting…") and its command arrives from the plugin later
    /// ([`App::handle_plugin_event`]). `session_id: None` is the picker's
    /// "New session".
    fn add_plugin_tab(&mut self, project: &Project, name: String, session_id: Option<String>) {
        let Some(plugin) = project.plugin.clone() else {
            return;
        };
        let status_id = new_status_id();
        let job = plugin::Job::Attach {
            project: plugin::payload(project),
            tab: status_id.clone(),
            session_id: session_id.clone(),
            fresh: session_id.is_none(),
            name: name.clone(),
        };
        let session = self.sessions.entry(project.name.clone()).or_default();
        session.tabs.push(Tab::Claude(ClaudeTab {
            name,
            // The plugin decides what runs; the harness is unused for its tabs.
            harness: HarnessKind::default(),
            session_id,
            fresh: false,
            id_reported: false,
            status_id,
            status: status::TabStatus::default(),
            pane: None,
            plugin_attach: PluginAttach::Pending,
            attach_when_shown: false,
        }));
        session.active_tab = session.tabs.len() - 1;
        self.persist_claude_sessions(&project.name);
        self.plugins.submit(&plugin, job);
    }

    /// Queue attaches for a plugin project's tabs that have no live pane, and
    /// for its shell, skipping any already in flight. Results come back as
    /// [`plugin::PluginEvent`]s ([`App::handle_plugin_event`]).
    fn request_plugin_panes(&mut self, project: &Project, plugin: &str) {
        let payload = plugin::payload(project);
        let Some(session) = self.sessions.get_mut(&project.name) else {
            return;
        };
        let mut jobs = Vec::new();
        for tab in session.claude_tabs_mut() {
            if tab.pane.as_mut().is_some_and(|p| p.child_finished()) {
                tab.pane = None;
            }
            if tab.pane.is_none() && tab.plugin_attach != PluginAttach::Pending {
                tab.plugin_attach = PluginAttach::Pending;
                jobs.push(plugin::Job::Attach {
                    project: payload.clone(),
                    tab: tab.status_id.clone(),
                    session_id: tab.session_id.clone(),
                    fresh: tab.fresh,
                    name: tab.name.clone(),
                });
                tab.fresh = false;
            }
        }
        if session.shell.as_mut().is_some_and(|p| p.child_finished()) {
            session.shell = None;
        }
        if session.shell.is_none() && !session.shell_pending {
            session.shell_pending = true;
            jobs.push(plugin::Job::Shell {
                project: payload,
                session_id: None,
            });
        }
        for job in jobs {
            self.plugins.submit(plugin, job);
        }
    }

    /// Apply a finished plugin job (drained from [`plugin::PluginHost`] each tick).
    fn handle_plugin_event(&mut self, event: plugin::PluginEvent, body: Rect) {
        match event {
            plugin::PluginEvent::Attached {
                plugin,
                project,
                tab,
                result,
            } => self.finish_plugin_attach(&plugin, &project, &tab, result, body),
            plugin::PluginEvent::Shell {
                plugin,
                project,
                session_id,
                result,
            } => self.finish_plugin_shell(&plugin, &project, session_id, result, body),
            plugin::PluginEvent::Sessions {
                plugin,
                project,
                result,
            } => self.show_plugin_sessions(&plugin, &project, result),
            plugin::PluginEvent::Projects { plugin, result } => {
                self.apply_listing(&plugin, result, body);
            }
            plugin::PluginEvent::Form {
                plugin,
                purpose,
                result,
            } => self.show_plugin_form(&plugin, purpose, result),
            plugin::PluginEvent::Removed { plugin, result } => {
                self.finish_plugin_remove(&plugin, result);
            }
            plugin::PluginEvent::Created {
                plugin,
                result,
                ticket: None,
            } => self.finish_plugin_create(&plugin, result),
            plugin::PluginEvent::Created {
                plugin,
                result,
                ticket: Some(ticket),
            } => self.finish_api_create(&plugin, ticket, result),
        }
    }

    /// Spawn a plugin tab's pane from its attach result, or record why not (the
    /// pane placeholder shows it).
    fn finish_plugin_attach(
        &mut self,
        plugin: &str,
        project_name: &str,
        status_id: &str,
        result: Result<plugin::AttachResponse>,
        body: Rect,
    ) {
        let Some(project) = self.project(project_name).cloned() else {
            return;
        };
        let Some(tab_count) = self.sessions.get(project_name).map(|s| s.tabs.len()) else {
            return;
        };
        let strip = self.tab_strip_shown(tab_count);
        let content = claude_pane_split(compute_layout(body, self).claude_inner(), strip).1;
        let mut env = self.base_pty_env(project_name);
        env.push(("WRK_TAB".to_string(), status_id.to_string()));
        let tab = self
            .sessions
            .get_mut(project_name)
            .and_then(|s| s.claude_tabs_mut().find(|t| t.status_id == status_id));
        let Some(tab) = tab else {
            // The tab closed mid-attach: release any session the plugin started.
            if let Ok(resp) = result
                && let Some(session_id) = resp.session_id
            {
                let job = plugin::Job::Close {
                    project: plugin::payload(&project),
                    session_id,
                };
                self.plugins.submit(plugin, job);
            }
            return;
        };
        let resp = match result {
            Ok(resp) => resp,
            Err(e) => {
                tab.plugin_attach = PluginAttach::Failed(format!("{e:#}"));
                return;
            }
        };
        let adopted = resp
            .session_id
            .filter(|id| tab.session_id.as_deref() != Some(id.as_str()));
        if let Some(id) = &adopted {
            tab.session_id = Some(id.clone());
        }
        env.extend(resp.env);
        let cwd = resp.cwd.map(PathBuf::from).unwrap_or(project.path);
        tab.id_reported = false;
        tab.plugin_attach =
            match PtyPane::spawn(&resp.command, &cwd, content.height, content.width, &env) {
                Ok(pane) => {
                    tab.pane = Some(pane);
                    PluginAttach::Idle
                }
                Err(e) => PluginAttach::Failed(format!("{e:#}")),
            };
        if adopted.is_some() {
            self.persist_claude_sessions(project_name);
        }
    }

    /// Spawn a plugin project's shell pane: the plugin's shell when it offered
    /// one, otherwise — including when the plugin failed — a local shell.
    fn finish_plugin_shell(
        &mut self,
        plugin: &str,
        project_name: &str,
        session_id: Option<String>,
        result: Result<Option<plugin::AttachResponse>>,
        body: Rect,
    ) {
        let Some(project) = self.project(project_name).cloned() else {
            return;
        };
        let shell_inner = compute_layout(body, self).shell_inner();
        let mut env = self.base_pty_env(project_name);
        let (command, cwd) = match result {
            Ok(Some(resp)) => {
                env.extend(resp.env);
                let cwd = resp.cwd.map(PathBuf::from).unwrap_or(project.path);
                (resp.command, cwd)
            }
            Ok(None) => (self.settings.shell(), project.path),
            Err(e) => {
                push_error(
                    &mut self.error,
                    format!("plugin '{plugin}': using a local shell ({e:#})"),
                );
                (self.settings.shell(), project.path)
            }
        };
        let Some(session) = self.sessions.get_mut(project_name) else {
            return;
        };
        session.shell_pending = false;
        session.shell_for = session_id;
        session.shell =
            match PtyPane::spawn(&command, &cwd, shell_inner.height, shell_inner.width, &env) {
                Ok(pane) => Some(pane),
                Err(e) => {
                    push_error(&mut self.error, format!("shell spawn failed: {e}"));
                    None
                }
            };
    }

    /// Fill the open new-tab picker with a plugin's session list, if it is
    /// still showing that plugin's project.
    fn show_plugin_sessions(
        &mut self,
        plugin: &str,
        project_name: &str,
        result: Result<Vec<plugin::PluginSession>>,
    ) {
        if self.active_project_name.as_deref() != Some(project_name) {
            return;
        }
        let Some(ModalState::ClaudeTabPicker(picker)) = self.modal.as_mut() else {
            return;
        };
        if picker.plugin_label() != Some(plugin) {
            return;
        }
        match result {
            Ok(sessions) => {
                let sessions: Vec<session::DiscoveredSession> = sessions
                    .into_iter()
                    .map(|s| session::DiscoveredSession {
                        session_id: s.id,
                        name: s.name,
                        // The plugin's order is kept as-is; there is no mtime.
                        modified: std::time::SystemTime::UNIX_EPOCH,
                    })
                    .collect();
                picker.set_sessions(&sessions);
            }
            Err(e) => picker.set_notice(Some(format!("couldn't list sessions: {e:#}"))),
        }
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
        // A file already open in this project is reloaded where it is (a
        // document written again, then opened again), not opened twice.
        let open_at = session.tabs.iter().position(|t| match (t, &tab) {
            (Tab::Markdown(open), Tab::Markdown(new)) => open.path == new.path,
            _ => false,
        });
        match open_at {
            Some(i) => {
                if let Tab::Markdown(open) = &mut session.tabs[i] {
                    open.reload();
                }
                session.active_tab = i;
            }
            None => {
                session.tabs.push(tab);
                session.active_tab = session.tabs.len() - 1;
            }
        }
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
    /// `reply` is its connection, which only a confirm answers on; the rest
    /// drop it, closing the connection.
    fn handle_request(&mut self, req: ipc::Request, reply: ipc::Reply) {
        match req {
            ipc::Request::Open(open) => self.handle_open_request(open),
            ipc::Request::Status(update) => self.handle_status_update(update),
            ipc::Request::Review(review) => self.handle_review_request(review),
            ipc::Request::Confirm(confirm) => {
                let source = confirm
                    .project
                    .as_deref()
                    .map(|name| self.project_label(name));
                self.prompts
                    .push_back(ui::prompt::PromptModal::new(confirm, source, reply));
                self.tick_prompts();
            }
            ipc::Request::Notify(note) => {
                let message = match note.project.as_deref() {
                    // A message about a project in the background says which.
                    Some(name) if self.active_project_name.as_deref() != Some(name) => {
                        format!("{}: {}", self.project_label(name), note.message)
                    }
                    _ => note.message,
                };
                if note.error {
                    push_error(&mut self.error, message);
                } else {
                    self.info = Some(message);
                }
            }
            // API requests are answered by `handle_incoming` (`api.rs`), which
            // never routes them here.
            ipc::Request::Sessions
            | ipc::Request::Send(_)
            | ipc::Request::Focus(_)
            | ipc::Request::Banner(_)
            | ipc::Request::Dismiss(_)
            | ipc::Request::Show(_)
            | ipc::Request::Hide
            | ipc::Request::Create(_) => {}
        }
    }

    /// How a project is named to the user: its display name, else the key.
    fn project_label(&self, name: &str) -> String {
        self.project(name)
            .map(|p| p.display_name().to_string())
            .unwrap_or_else(|| name.to_string())
    }

    /// What programs in any pane (every session's tabs and shell, and the
    /// widgets) copied with OSC 52 since the last call, for the host
    /// terminal's clipboard. Background panes count too, as in a terminal.
    fn take_copied(&self) -> Vec<pane::terminal::Copied> {
        let sessions = self.sessions.values().flat_map(|s| {
            s.claude_tabs()
                .filter_map(|t| t.pane.as_ref())
                .chain(s.shell.as_ref())
        });
        let widgets = self.widgets.instances.iter().map(|w| &w.pane);
        sessions
            .chain(widgets)
            .flat_map(|p| p.take_copied())
            .collect()
    }

    /// Keep `wrk confirm` prompts moving: drop any whose requester hung up
    /// (closing the one on screen with a note), and show the next once no
    /// other modal is open.
    fn tick_prompts(&mut self) {
        if let Some(ModalState::Prompt(m)) = &self.modal
            && m.reply.withdrawn()
        {
            let who = m.source.clone().unwrap_or_else(|| "a pane".into());
            self.modal = None;
            self.info = Some(format!("{who} withdrew its question"));
        }
        self.prompts.retain(|m| !m.reply.withdrawn());
        if self.modal.is_none()
            && let Some(mut next) = self.prompts.pop_front()
        {
            next.show(Instant::now());
            self.modal = Some(ModalState::Prompt(next));
        }
    }

    /// Handle a `wrk review` request: `Start` computes the diff for the named
    /// project and opens the full-screen review overlay; `End` closes it. Errors
    /// surface in the status bar rather than interrupting the session.
    fn handle_review_request(&mut self, req: ipc::ReviewRequest) {
        match req.kind {
            ipc::ReviewKind::Start { target } => {
                // Validate before any git runs: a revision starting with `-`
                // would reach git as an option (#95). The CLI checks too, but
                // anything can write to the socket.
                let target = match review::diff::DiffTarget::parse(target.as_deref().unwrap_or(""))
                {
                    Ok(target) => target,
                    Err(e) => {
                        push_error(&mut self.error, format!("wrk review: {e}"));
                        return;
                    }
                };
                let project = req.project.or_else(|| self.active_project_name.clone());
                let Some(project) = project else {
                    push_error(&mut self.error, "wrk review: no active project".into());
                    return;
                };
                let Some(path) = self.project(&project).map(|p| p.path.clone()) else {
                    push_error(
                        &mut self.error,
                        format!("wrk review: unknown project '{project}'"),
                    );
                    return;
                };
                // Diff the git repo enclosing the directory `wrk review start`
                // ran in (its worktree/repo root), falling back to the project
                // path. The project dir itself may be a non-git container of many
                // clones/worktrees, so running git there fails (#94).
                let start_dir = req
                    .cwd
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from);
                let repo_root = match &start_dir {
                    Some(dir) => review::diff::resolve_repo_root(dir)
                        .or_else(|_| review::diff::resolve_repo_root(&path)),
                    None => review::diff::resolve_repo_root(&path),
                };
                let repo_root = match repo_root {
                    Ok(root) => root,
                    Err(e) => {
                        push_error(&mut self.error, format!("wrk review: {e}"));
                        return;
                    }
                };
                match review::diff::build_review(&repo_root, &target) {
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
        // Find the tab this push targets and fold in the status kind. When the
        // push carries a session id that differs from the tab's, the tab now
        // hosts a different session (Claude `/clear`/`/resume`, or a Kimi id
        // learned for the first time): adopt it so a restart resumes it, rename
        // the tab if it's a genuinely different conversation, and persist.
        let mut changed_project: Option<String> = None;
        'outer: for (project_name, session) in self.sessions.iter_mut() {
            for tab in session.claude_tabs_mut() {
                if tab.status_id != update.tab {
                    continue;
                }
                tab.status.apply(update.kind);
                let first_report = !tab.id_reported;
                if update.session_id.is_some() {
                    tab.id_reported = true;
                }
                if let Some(id) = &update.session_id
                    && tab.session_id.as_deref() != Some(id.as_str())
                {
                    let continued = first_report
                        || tab.session_id.is_none()
                        || update.source.as_deref() == Some("compact");
                    let empty = BTreeMap::new();
                    let names = self
                        .store
                        .find(project_name)
                        .map_or(&empty, |p| &p.session_names);
                    if let Some(name) = switched_tab_name(&tab.name, id, names, continued) {
                        tab.name = name;
                    }
                    tab.session_id = Some(id.clone());
                    changed_project = Some(project_name.clone());
                }
                break 'outer;
            }
        }
        if let Some(project_name) = changed_project {
            self.persist_claude_sessions(&project_name);
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
        let remote = self.project(&project_name).is_some_and(Project::is_remote);
        let Some(session) = self.sessions.get_mut(&project_name) else {
            return;
        };
        // A listed project's tab mirrors a session that lives on with its
        // plugin: closing it only detaches the pane. The tab stays listed.
        if remote && let Some(Tab::Claude(tab)) = session.current_mut() {
            tab.pane = None;
            tab.plugin_attach = PluginAttach::Idle;
            tab.attach_when_shown = false;
            self.info = Some(format!("detached '{}'; it keeps running", tab.name));
            return;
        }
        let Some(tab) = session.current() else {
            return;
        };
        let removed_claude = matches!(tab, Tab::Claude(_));
        if removed_claude && session.claude_tabs().count() <= 1 {
            return;
        }
        // Capture the closing session's id (if any) before the tab is dropped so
        // a plugin-backed project can be told it closed.
        let closed_session_id = tab.as_claude().and_then(|t| t.session_id.clone());
        session.tabs.remove(session.active_tab);
        if session.active_tab >= session.tabs.len() {
            session.active_tab = session.tabs.len().saturating_sub(1);
        }
        if removed_claude {
            self.persist_claude_sessions(&project_name);
            self.notify_plugin_close(&project_name, closed_session_id);
        }
    }

    /// Best-effort notice to a plugin that a tab hosting one of its sessions
    /// closed. No-op for internal projects or a tab with no session id; it runs
    /// off the UI thread and never launches the plugin just to say goodbye.
    fn notify_plugin_close(&mut self, project_name: &str, session_id: Option<String>) {
        let Some(session_id) = session_id else {
            return;
        };
        let Some(project) = self.project(project_name) else {
            return;
        };
        let Some(plugin) = project.plugin.clone() else {
            return;
        };
        let job = plugin::Job::Close {
            project: plugin::payload(project),
            session_id,
        };
        self.plugins.submit(&plugin, job);
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
                    harness: t.harness,
                })
                .collect::<Vec<_>>(),
            None => return,
        };
        let Some(p) = self
            .store
            .projects
            .iter_mut()
            .find(|p| p.name == project_name)
        else {
            // No matching project in the store (e.g. a session with no persisted
            // project) — nothing to write. Skip the save so we never clobber the
            // config with an unrelated snapshot.
            return;
        };
        // Persist the live tab list faithfully. A Claude tab keeps `session_id =
        // None` only until its first spawn (a UUID is minted then); a Kimi tab
        // until its SessionStart hook reveals the id — after which restarts resume.
        p.claude_sessions = tabs;
        p.remember_live_session_names();
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
        } else if let Some(p) = self.remote.find_mut(&name) {
            // A listed project's choice lasts the run; it is never saved.
            p.layout_mode = Some(mode);
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
        } else if let Some(p) = self.remote.find_mut(&name) {
            p.shell_passthrough = to_store;
        }
        if changed && let Err(e) = store::save(&self.store) {
            push_error(&mut self.error, format!("save failed: {e}"));
        }
    }
}

/// Make a listed project's open tabs match its latest listing: the listed
/// sessions in listing order — keeping each one's tab (and live pane), with
/// the listed name — plus new tabs (unattached until shown) for new sessions.
/// A tab whose session is no longer listed is dropped, unless its pane is
/// still running or attaching. Markdown tabs follow, as they were. The shown
/// tab stays shown when it survives.
fn reconcile_remote_tabs(session: &mut ProjectSession, listed: &[SessionRef]) {
    fn identity(tab: &Tab) -> String {
        match tab {
            Tab::Claude(t) => format!("c:{}", t.status_id),
            Tab::Markdown(m) => format!("m:{}", m.path.display()),
        }
    }
    let shown = session.current().map(identity);
    let mut claude = Vec::new();
    let mut markdown = Vec::new();
    for tab in std::mem::take(&mut session.tabs) {
        match tab {
            Tab::Claude(t) => claude.push(t),
            Tab::Markdown(m) => markdown.push(m),
        }
    }
    let mut tabs = Vec::new();
    for sr in listed {
        let tab = match claude.iter().position(|t| t.session_id == sr.session_id) {
            Some(i) => {
                let mut tab = claude.remove(i);
                tab.name = sr.name.clone();
                tab
            }
            None => ClaudeTab {
                name: sr.name.clone(),
                harness: sr.harness,
                session_id: sr.session_id.clone(),
                fresh: false,
                id_reported: false,
                status_id: new_status_id(),
                status: status::TabStatus::default(),
                pane: None,
                plugin_attach: PluginAttach::Idle,
                attach_when_shown: true,
            },
        };
        tabs.push(Tab::Claude(tab));
    }
    for tab in claude {
        if tab.pane.is_some() || tab.plugin_attach == PluginAttach::Pending {
            tabs.push(Tab::Claude(tab));
        }
    }
    tabs.extend(markdown.into_iter().map(Tab::Markdown));
    let at = shown.and_then(|id| tabs.iter().position(|t| identity(t) == id));
    session.active_tab = at.unwrap_or(session.active_tab.min(tabs.len().saturating_sub(1)));
    session.tabs = tabs;
}

/// The new name for a tab whose session id just changed to `new_id`, or `None`
/// to keep `current`. A session wrk already knows keeps its recorded name; a
/// `continued` conversation (same one under a new id) keeps the tab's name; a
/// genuinely new one gets a fresh ` (N)` suffix.
fn switched_tab_name(
    current: &str,
    new_id: &str,
    session_names: &BTreeMap<String, String>,
    continued: bool,
) -> Option<String> {
    if let Some(known) = session_names.get(new_id) {
        return Some(known.clone());
    }
    (!continued).then(|| next_session_name(current, session_names))
}

/// Name for a tab that switched to a new conversation: `current`'s base (any
/// trailing ` (N)` stripped) with the lowest ` (N)`, N ≥ 2, that no recorded
/// session already uses — so the old session keeps its name in the picker.
fn next_session_name(current: &str, session_names: &BTreeMap<String, String>) -> String {
    let base = current
        .strip_suffix(')')
        .and_then(|s| s.rsplit_once(" ("))
        .filter(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .map_or(current, |(b, _)| b);
    let taken = |name: &str| session_names.values().any(|v| v == name);
    (2..)
        .map(|n| format!("{base} ({n})"))
        .find(|name| !taken(name))
        .expect("unbounded range")
}

/// A fresh Claude session UUID (v4) for a brand-new tab.
fn new_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Build the command to **resume** an existing agent session for `harness`.
///
/// - Claude: `claude --resume <id>`. `claude --continue` is intentionally not
///   used — it resumes "the most recent session in this directory", which is
///   non-deterministic when multiple wrk projects share a path.
/// - Kimi: `kimi --session <id>`. Kimi assigns the id itself; wrk learns it from
///   the `SessionStart`/status hooks and persists it, so resumption is likewise
///   pinned to this tab's own session.
fn resume_agent_command(
    settings: &Settings,
    harness: HarnessKind,
    session_id: &str,
) -> Vec<String> {
    let mut cmd = settings.harness_command(harness);
    match harness {
        HarnessKind::Claude => cmd.push("--resume".to_string()),
        HarnessKind::Kimi => cmd.push("--session".to_string()),
    }
    cmd.push(session_id.to_string());
    cmd
}

/// Build the command to **continue** the most recent session in the working
/// directory: `kimi --continue`. Used for Kimi tabs that have no learned session
/// id (the common case, since Kimi 2.0.0 doesn't fire the hooks that would reveal
/// one) — `--continue` resumes the directory's latest session, or starts a fresh
/// one if there is none. Not used for Claude (its resume is id-pinned).
fn continue_agent_command(settings: &Settings, harness: HarnessKind) -> Vec<String> {
    let mut cmd = settings.harness_command(harness);
    cmd.push("--continue".to_string());
    cmd
}

/// Build the command to **start a new** agent session for `harness`.
///
/// - Claude: `claude --session-id <id> --name <name>`. Assigning the UUID up
///   front (rather than guessing it from the filesystem afterwards) is what makes
///   resumption deterministic — the tab is bound to its own session from launch.
///   `--name` labels it after its project. Callers pass a freshly minted UUID.
/// - Kimi: bare `kimi`. Kimi mints its own session id and reveals it via the
///   `SessionStart` hook, so wrk doesn't pass one in; `session_id`/`name` are
///   unused here (the hook-pushed id gets persisted once the session starts).
fn new_agent_command(
    settings: &Settings,
    harness: HarnessKind,
    session_id: &str,
    name: &str,
) -> Vec<String> {
    let mut cmd = settings.harness_command(harness);
    match harness {
        HarnessKind::Claude => {
            cmd.push("--session-id".to_string());
            cmd.push(session_id.to_string());
            cmd.push("--name".to_string());
            cmd.push(name.to_string());
        }
        HarnessKind::Kimi => {}
    }
    cmd
}

/// Build the launch command for `tab`, mutating it as needed. Claude mints and
/// records a session UUID on first spawn (setting `*assigned_new` so the caller
/// persists it). Kimi: a learned session id resumes precisely (`--session`); a
/// just-created (`fresh`) tab starts a new session (bare `kimi`); any other Kimi
/// tab continues the directory's latest session (`--continue`). `fresh` is
/// cleared afterwards so a later respawn of a live tab resumes, not restarts.
fn spawn_command_for_tab(
    settings: &Settings,
    tab: &mut ClaudeTab,
    assigned_new: &mut bool,
) -> Vec<String> {
    let cmd = match (tab.session_id.clone(), tab.harness) {
        (Some(id), h) => resume_agent_command(settings, h, &id),
        (None, HarnessKind::Claude) => {
            let id = new_session_id();
            let cmd = new_agent_command(settings, HarnessKind::Claude, &id, &tab.name);
            tab.session_id = Some(id);
            *assigned_new = true;
            cmd
        }
        (None, HarnessKind::Kimi) if tab.fresh => {
            new_agent_command(settings, HarnessKind::Kimi, "", &tab.name)
        }
        (None, HarnessKind::Kimi) => continue_agent_command(settings, HarnessKind::Kimi),
    };
    tab.fresh = false;
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
/// `allowWindowOps`/`disallowedWindowOps`; inside tmux, `set -g set-clipboard on`).
fn copy_to_clipboard(text: &str) {
    write_to_host(&osc52(text, false));
}

/// The OSC 52 sequence that puts `text` on the clipboard, or on the primary
/// selection when `primary`.
fn osc52(text: &str, primary: bool) -> String {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let target = if primary { 'p' } else { 'c' };
    format!("\x1b]52;{target};{encoded}\x07")
}

/// Pass what programs in panes copied on to the host terminal, as a terminal
/// does. A program in a remote session can't reach a clipboard any other way.
fn forward_copied(app: &App) {
    for c in app.take_copied() {
        write_to_host(&osc52(&c.text, c.primary));
    }
}

/// Write an escape sequence straight to the host terminal, between frames.
fn write_to_host(seq: &str) {
    use std::io::Write;
    let mut stdout = io::stdout();
    let _ = stdout.write_all(seq.as_bytes());
    let _ = stdout.flush();
}

fn run_tui() -> Result<()> {
    let store = store::load()?;
    let settings = settings::load().unwrap_or_else(|e| {
        eprintln!("warning: failed to load settings.toml: {e}; using defaults");
        Settings::default()
    });
    let mut app = App::new(store, settings);

    // Reclaim runtime artifacts leaked by instances that exited uncleanly
    // (sockets and review-comment mirrors whose owning pid is gone), so stale
    // files don't accumulate or feed a later `wrk review end` old comments.
    ipc::sweep_stale_sockets();
    review::sweep_stale_mirrors();

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
    app.plugins.set_wrk_sock(socket_path.as_deref());

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

    // Widgets that run from the start (hidden), so they can notify before
    // they're first summoned. After the socket exists: they get `WRK_SOCK`.
    if let Ok(size) = terminal.size() {
        app.autostart_widgets(body_rect(size.into()));
    }

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
    ipc_rx: Option<Receiver<ipc::Incoming>>,
) -> Result<()> {
    while !app.should_quit {
        app.poll_remote();
        app.ensure_remote_panes();
        app.refresh_sidebar();
        terminal.draw(|frame| ui::draw(frame, app))?;

        let area: Rect = terminal.size().context("terminal size")?.into();
        let body = body_rect(area);
        let layout = compute_layout(body, app);
        let claude_inner = layout.claude_inner();
        let shell_inner = layout.shell_inner();

        // Resize only the active session's panes; inactive sessions keep their grids.
        let claude_content = app.active_primary_split(claude_inner).1;
        if let Some(session) = app.active_session_mut() {
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

        // Drain IPC requests: `wrk view` open-file requests, `wrk hook` status
        // pushes, `wrk review`, `wrk confirm` prompts, `wrk notify` messages,
        // and API calls (answered).
        if let Some(rx) = &ipc_rx {
            while let Ok(incoming) = rx.try_recv() {
                app.handle_incoming(incoming, body);
            }
        }
        app.tick_prompts();
        let now = Instant::now();
        app.flush_pending_submits(now);
        app.notifications.expire(now);
        app.tick_widgets(body);

        // Drain finished session-plugin jobs (attach / shell / list run off-thread).
        while let Some(event) = app.plugins.try_recv() {
            app.handle_plugin_event(event, body);
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

        // Before reaping, so a program that copies and exits still lands.
        forward_copied(app);

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
/// strip is only present when `strip` asks for it (see
/// [`App::tab_strip_shown`]) and the inner area is at least 2 rows tall.
///
/// Shared by the renderer (`ui::draw_claude_pane`), the resize loop, the PTY
/// spawn sites, and the mouse click router so they all agree on where the
/// strip lives and how much room is left for the embedded terminal.
pub fn claude_pane_split(inner: Rect, strip: bool) -> (Option<Rect>, Rect) {
    if strip && inner.height >= 2 {
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

    // A widget on screen gets the keyboard (bar its own hide/switch keys).
    if app.widgets.shown.is_some() {
        app.widget_key(key, body);
        return Ok(());
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

    // A widget's own summon key (`[widget.<name>] key`). Global shortcuts win;
    // a conflicting widget key was dropped with a warning at startup.
    if action.is_none()
        && let Some(name) = app
            .widget_keys
            .get(&KeyEvent::new(key.code, key.modifiers))
            .cloned()
    {
        if let Err(e) = app.show_widget(&name, body) {
            push_error(&mut app.error, e);
        }
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
        // Horizontal scroll for lines wider than the pane.
        KeyCode::Char('<') | KeyCode::Char(',') => review.scroll_h(-8),
        KeyCode::Char('>') | KeyCode::Char('.') => review.scroll_h(8),
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
            // A listed project's new sessions are made with its plugin's form.
            if let Some((plugin, key)) = app
                .active_project()
                .and_then(|p| Some((p.remote.as_ref()?.plugin.clone(), p.name.clone())))
            {
                app.open_plugin_form(&plugin, Some(key));
                return true;
            }
            // A plugin project's sessions load off the UI thread into the open
            // picker (`show_plugin_sessions`); an internal one's come from disk.
            let plugin_list = app
                .active_project()
                .and_then(|p| Some((p.plugin.clone()?, plugin::payload(p))));
            let modal = match plugin_list {
                Some((plugin, project)) => {
                    app.plugins.submit(&plugin, plugin::Job::List { project });
                    ClaudeTabPickerModal::new_for_plugin(plugin)
                }
                None => {
                    let discovered = app
                        .active_project()
                        .map(|p| session::discover_sessions_named(&p.path, &p.session_names))
                        .unwrap_or_default();
                    ClaudeTabPickerModal::new(
                        &discovered,
                        app.settings.enabled_harnesses(),
                        app.settings.default_harness(),
                    )
                }
            };
            app.modal = Some(ModalState::ClaudeTabPicker(modal));
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
        GlobalAction::Widgets => app.toggle_widgets(body),
        // With no banner up these keys belong to the focused program.
        GlobalAction::OpenNotification => return app.open_notification(body),
        GlobalAction::ClearNotification => return app.notifications.pop_newest().is_some(),
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
            ModalState::Add(m) => {
                if let Some(input) = m.current_input_mut() {
                    input.push_str(&payload);
                }
            }
            ModalState::OpenMarkdown(m) => m.path_input.push_str(&payload),
            ModalState::ClaudeTabPicker(m) if m.name_focused => m.tab_name.push_str(&payload),
            ModalState::PluginForm(m) => {
                if let Some(input) = m.input_mut() {
                    // One line: an Enter in the paste isn't a submit.
                    input.push_str(payload.trim_end_matches('\r').replace('\r', " ").as_str());
                }
            }
            ModalState::UrlPicker(m) | ModalState::WidgetPicker(m) => {
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

    if let Some(w) = app.widgets.shown_mut() {
        if w.exited.is_none() {
            let bytes = paste_bytes(&w.pane, &payload);
            let _ = w.pane.write(&bytes);
        }
        return;
    }

    if app.focus == Focus::Projects {
        if let Some(filter) = app.sidebar.filter.as_mut() {
            filter.push_str(&payload);
            app.refresh_sidebar();
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

/// The bytes that paste `payload` (already [`normalize_paste`]d) into `pane`:
/// wrapped in bracketed-paste markers when the program asked for them.
fn paste_bytes(pane: &PtyPane, payload: &str) -> Vec<u8> {
    if pane.bracketed_paste_mode() {
        let mut v = Vec::with_capacity(payload.len() + 12);
        v.extend_from_slice(b"\x1b[200~");
        v.extend_from_slice(payload.as_bytes());
        v.extend_from_slice(b"\x1b[201~");
        v
    } else {
        payload.as_bytes().to_vec()
    }
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
    let bytes = paste_bytes(pane, payload);
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

/// A plugin form's values, on their way to the plugin.
struct FormSubmission {
    plugin: String,
    context: Option<String>,
    purpose: plugin::FormFor,
    session_id: Option<String>,
    values: BTreeMap<String, String>,
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
        KeyCode::Char('+') => match app.sidebar.selected_group().map(str::to_string) {
            // In a plugin's group: create with its form, from the selected project.
            Some(group) if group != SidebarGroup::LOCAL => {
                let context = app.sidebar.selected_project().map(str::to_string);
                app.open_plugin_form(&group, context);
            }
            _ => {
                app.modal = Some(ModalState::Add(AddProjectModal::new(
                    app.settings.plugin_names(),
                )));
            }
        },
        KeyCode::Char('d') => {
            if let Some((name, tab)) = app.sidebar.selected_target()
                && let Some(p) = app.project(&name)
            {
                if p.remote.is_some() {
                    app.remove_listed(&name, tab);
                } else {
                    app.modal = Some(ModalState::ConfirmDelete(ConfirmDeleteModal {
                        project_name: p.name.clone(),
                    }));
                }
            }
        }
        KeyCode::Char('u') => {
            if let Some(name) = app.sidebar.selected_project()
                && let Some(p) = app.project(name)
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
            app.refresh_sidebar();
        }
        KeyCode::Char('r') => {
            app.reload_store()?;
            app.remote.refresh_all();
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
            app.refresh_sidebar();
        }
        KeyCode::Enter => {
            app.open_selected(body);
            app.sidebar.filter = None;
            app.refresh_sidebar();
        }
        KeyCode::Backspace => {
            filter.pop();
            app.refresh_sidebar();
        }
        KeyCode::Char(c) => {
            filter.push(c);
            app.refresh_sidebar();
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
    // Deferred like `open_md_input`: (plugin, the form's project, values).
    let mut submitted: Option<FormSubmission> = None;
    match app.modal.as_mut().unwrap() {
        ModalState::Add(m) => match key.code {
            KeyCode::Esc => {
                consumed_modal = Some(());
            }
            KeyCode::Tab => m.toggle_focus(),
            // ←/→ cycle the provider when its row is focused.
            KeyCode::Left if m.focus == AddField::Provider => {
                m.cycle_provider(-1);
            }
            KeyCode::Right if m.focus == AddField::Provider => {
                m.cycle_provider(1);
            }
            KeyCode::Backspace => {
                if let Some(input) = m.current_input_mut() {
                    input.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(input) = m.current_input_mut() {
                    input.push(c);
                }
            }
            KeyCode::Enter => {
                let path = expand_tilde(m.path_input.trim());
                let name_input = m.name_input.trim().to_string();
                let provider = m.selected_provider();
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
                        session_names: BTreeMap::new(),
                        plugin: provider,
                        plugin_config: BTreeMap::new(),
                        remote: None,
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
                    // ←/→ cycle the harness when more than one is enabled.
                    KeyCode::Left => m.cycle_harness(-1),
                    KeyCode::Right => m.cycle_harness(1),
                    KeyCode::Enter => {
                        m.confirmed = true;
                        consumed_modal = Some(());
                    }
                    _ => {}
                }
            }
        }
        ModalState::Prompt(m) => {
            // Keys in its first moments were meant for the pane: ignored.
            if m.armed(Instant::now()) {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        m.answer(true);
                        consumed_modal = Some(());
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        m.answer(false);
                        consumed_modal = Some(());
                    }
                    _ => {}
                }
            }
        }
        ModalState::PluginForm(m) => match key.code {
            // Closing while a create runs leaves it running; its result still
            // refreshes the listing and opens what it made.
            KeyCode::Esc => consumed_modal = Some(()),
            KeyCode::Tab | KeyCode::Down => m.focus_step(1),
            KeyCode::BackTab | KeyCode::Up => m.focus_step(-1),
            KeyCode::Left => m.cycle(-1),
            KeyCode::Right => m.cycle(1),
            KeyCode::Backspace => {
                if let Some(input) = m.input_mut() {
                    input.pop();
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(input) = m.input_mut() {
                    input.push(c);
                }
            }
            KeyCode::Enter if !m.submitting && m.form.is_some() => match m.submission() {
                Ok(values) => {
                    m.submitting = true;
                    m.error = None;
                    submitted = Some(FormSubmission {
                        plugin: m.plugin.clone(),
                        context: m.context.clone(),
                        purpose: m.purpose,
                        session_id: m.session_id.clone(),
                        values,
                    });
                }
                Err(e) => m.error = Some(e),
            },
            _ => {}
        },
        ModalState::UrlPicker(m) | ModalState::WidgetPicker(m) => match key.code {
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
                app.add_claude_tab(name, m.harness, session_id, body);
            }
            Some(ModalState::WidgetPicker(m)) => {
                // The picker lists labels in `widget_names` order.
                let name = m
                    .confirmed_url
                    .and_then(|label| m.all.iter().position(|l| *l == label))
                    .and_then(|i| app.widget_names().get(i).cloned());
                if let Some(name) = name
                    && let Err(e) = app.show_widget(&name, body)
                {
                    push_error(&mut app.error, e);
                }
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
    if let Some(sent) = submitted {
        let project = sent
            .context
            .as_deref()
            .and_then(|key| app.remote.find(key))
            .map(plugin::payload);
        let job = match (sent.purpose, project) {
            (plugin::FormFor::Create, project) => Some(plugin::Job::Create {
                project,
                values: sent.values,
                ticket: None,
            }),
            (plugin::FormFor::Remove, Some(project)) => Some(plugin::Job::Remove {
                project,
                session_id: sent.session_id,
                values: sent.values,
            }),
            // Its project left the listing while the form was open.
            (plugin::FormFor::Remove, None) => None,
        };
        match job {
            Some(job) => app.plugins.submit(&sent.plugin, job),
            None => {
                app.modal = None;
                app.info = Some("it's gone already".into());
            }
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

    // The fullscreen review overlay captures the mouse: clicks select files /
    // diff rows (hit-tested against the last render), the wheel scrolls
    // whichever pane the pointer is over.
    if let Some(review) = app.review.as_mut() {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => review.click(m.column, m.row),
            MouseEventKind::ScrollDown => review.wheel(m.column, m.row, true),
            MouseEventKind::ScrollUp => review.wheel(m.column, m.row, false),
            // Horizontal wheel (or Shift+wheel in most terminals) pans long
            // lines, like `<`/`>`.
            MouseEventKind::ScrollLeft => review.scroll_h(-8),
            MouseEventKind::ScrollRight => review.scroll_h(8),
            _ => {}
        }
        return;
    }

    if app.modal.is_some() {
        return;
    }

    let body = body_rect(area);

    // The notification banner on the top row: a click on its ✕ dismisses it,
    // anywhere else opens what it points at.
    if !app.notifications.is_empty() && m.row == area.y {
        if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
            if m.column + notifications::CLOSE_HIT_WIDTH >= area.right() {
                app.notifications.pop_newest();
            } else {
                app.open_notification(body);
            }
        }
        return;
    }

    if app.widgets.shown.is_some() {
        handle_widget_mouse(app, m, body);
        return;
    }

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

    // Sidebar clicks always belong to us, never to a PTY. The wheel scrolls
    // the list without moving the selection.
    if let Some(sidebar) = layout.sidebar
        && rect_contains(sidebar, pos_x, pos_y)
    {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                handle_sidebar_click(app, sidebar, pos_y, body);
            }
            MouseEventKind::ScrollDown => app.sidebar.scroll_by(sidebar, SCROLL_LINES as isize),
            MouseEventKind::ScrollUp => app.sidebar.scroll_by(sidebar, -(SCROLL_LINES as isize)),
            _ => {}
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

/// A mouse event while a widget is on screen. Inside it, events go to the
/// widget's program when it asked for mouse reporting, else the wheel scrolls
/// its scrollback. A click outside hides it (like dismissing a popup).
fn handle_widget_mouse(app: &mut App, m: MouseEvent, body: Rect) {
    let outer = widget::overlay_rect(body);
    let inner = widget::overlay_inner(body);
    let down = matches!(m.kind, MouseEventKind::Down(MouseButton::Left));
    if !rect_contains(outer, m.column, m.row) {
        if down {
            app.hide_widget();
        }
        return;
    }
    let Some(w) = app.widgets.shown_mut() else {
        return;
    };
    if w.exited.is_some() {
        if down {
            app.hide_widget();
        }
        return;
    }
    if !rect_contains(inner, m.column, m.row) {
        return; // the border
    }
    let mode = w.pane.mouse_mode();
    let forward = match m.kind {
        MouseEventKind::Down(_) | MouseEventKind::Up(_) => mode.any(),
        MouseEventKind::Drag(_) => mode.drag || mode.motion,
        MouseEventKind::Moved => mode.motion,
        MouseEventKind::ScrollUp
        | MouseEventKind::ScrollDown
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => mode.any(),
    };
    if forward {
        let (cx, cy) = (m.column - inner.x + 1, m.row - inner.y + 1);
        let bytes = pane::mouse_to_bytes(m, cx, cy, mode);
        if !bytes.is_empty() {
            let _ = w.pane.write(&bytes);
        }
        return;
    }
    match m.kind {
        MouseEventKind::ScrollUp => w.pane.scroll(SCROLL_LINES),
        MouseEventKind::ScrollDown => w.pane.scroll(-SCROLL_LINES),
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
        app.active_primary_split(inset(outer)).1
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
        app.active_primary_split(inset(outer)).1
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
        app.active_primary_split(inset(outer)).1
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
        app.active_primary_split(inset(outer)).1
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
    let Some(visible_idx) = app.sidebar.row_at(sidebar, row) else {
        return;
    };
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
/// it is shown at all (sidebar hidden, at least one tab) and the claude pane
/// is visible (always in Split, only when focus is not Shell in Tabbed).
fn visible_claude_tab_strip(app: &App, layout: &LayoutRects) -> Option<Rect> {
    if app.layout_mode == LayoutMode::Tabbed && app.focus == Focus::Shell {
        return None;
    }
    app.active_primary_split(inset(layout.claude)).0
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
        App, ClaudeTab, Focus, HarnessKind, MarkdownTab, ModalState, PluginAttach, PluginFormModal,
        Project, ProjectSession, ProjectStore, Settings, Tab, continue_agent_command, expand_tilde,
        handle_modal_key, handle_projects_key, new_agent_command, new_session_id, normalize_paste,
        resume_agent_command,
    };
    use crate::settings::HarnessConfig;
    use crate::{ipc, plugin, ui};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;
    use std::collections::BTreeMap;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::time::Instant;

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

    fn confirm(title: &str) -> ipc::ConfirmRequest {
        ipc::ConfirmRequest {
            project: Some("web".into()),
            title: title.into(),
            lines: vec!["a.md".into()],
            accept: Some("download".into()),
            decline: None,
        }
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    /// The prompt on screen, made old enough to take keys.
    fn arm_prompt(app: &mut App) {
        let Some(ModalState::Prompt(m)) = app.modal.as_mut() else {
            panic!("no prompt on screen");
        };
        m.shown_at = Some(Instant::now() - ui::prompt::ARM_AFTER);
    }

    fn answer(theirs: &UnixStream) -> Option<bool> {
        let mut line = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(theirs), &mut line).unwrap();
        (!line.is_empty()).then(|| {
            serde_json::from_str::<ipc::ConfirmReply>(line.trim())
                .unwrap()
                .accepted
        })
    }

    #[test]
    fn confirm_prompts_queue_and_answer_on_their_own_connections() {
        let body = Rect::new(0, 0, 100, 40);
        let mut app = empty_app();
        let (first_ours, first_theirs) = UnixStream::pair().unwrap();
        let (second_ours, second_theirs) = UnixStream::pair().unwrap();
        app.handle_request(
            ipc::Request::Confirm(confirm("first?")),
            ipc::Reply::new(first_ours),
        );
        app.handle_request(
            ipc::Request::Confirm(confirm("second?")),
            ipc::Reply::new(second_ours),
        );
        let Some(ModalState::Prompt(m)) = &app.modal else {
            panic!("the first prompt shows at once");
        };
        assert_eq!(m.title, "first?");
        assert_eq!(app.prompts.len(), 1, "the second waits its turn");

        // A key in its first moments was meant for a pane: ignored.
        handle_modal_key(&mut app, key('y'), body).unwrap();
        assert!(matches!(app.modal, Some(ModalState::Prompt(_))));

        arm_prompt(&mut app);
        handle_modal_key(&mut app, key('y'), body).unwrap();
        assert_eq!(answer(&first_theirs), Some(true));
        assert!(app.modal.is_none());

        app.tick_prompts();
        let Some(ModalState::Prompt(m)) = &app.modal else {
            panic!("the second prompt follows");
        };
        assert_eq!(m.title, "second?");
        arm_prompt(&mut app);
        handle_modal_key(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            body,
        )
        .unwrap();
        assert_eq!(answer(&second_theirs), Some(false));
    }

    #[test]
    fn a_prompt_whose_asker_hangs_up_goes_away() {
        let mut app = empty_app();
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (queued_ours, queued_theirs) = UnixStream::pair().unwrap();
        app.handle_request(
            ipc::Request::Confirm(confirm("one?")),
            ipc::Reply::new(ours),
        );
        app.handle_request(
            ipc::Request::Confirm(confirm("two?")),
            ipc::Reply::new(queued_ours),
        );
        drop(queued_theirs);
        drop(theirs);
        // A concurrent test forking a child briefly holds copies of every fd
        // (until its exec), which can delay the EOF a hang-up shows as: poll.
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        app.tick_prompts();
        while (app.modal.is_some() || !app.prompts.is_empty()) && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
            app.tick_prompts();
        }
        assert!(app.modal.is_none(), "both withdrew");
        assert!(app.prompts.is_empty());
        assert_eq!(app.info.as_deref(), Some("web withdrew its question"));
    }

    #[test]
    fn notify_shows_in_the_status_bar_naming_a_background_project() {
        let mut app = empty_app();
        app.active_project_name = Some("api".into());
        let note = |message: &str, error| {
            ipc::Request::Notify(ipc::NotifyRequest {
                project: Some("web".into()),
                message: message.into(),
                error,
            })
        };
        app.handle_request(note("saved 2 files", false), ipc::Reply::default());
        assert_eq!(app.info.as_deref(), Some("web: saved 2 files"));
        app.handle_request(note("transfer failed", true), ipc::Reply::default());
        assert_eq!(app.error.as_deref(), Some("web: transfer failed"));
    }

    #[test]
    fn opening_an_open_file_again_reloads_it_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.md");
        std::fs::write(&path, "# one").unwrap();
        let mut app = empty_app();
        app.sessions.insert(
            "web".to_string(),
            ProjectSession {
                tabs: vec![claude_tab("c")],
                ..Default::default()
            },
        );
        app.add_markdown_tab("web", path.clone()).unwrap();
        app.sessions.get_mut("web").unwrap().active_tab = 0;
        std::fs::write(&path, "# two").unwrap();
        app.add_markdown_tab("web", path.clone()).unwrap();
        let session = &app.sessions["web"];
        assert_eq!(session.tabs.len(), 2, "no second tab for the same file");
        assert_eq!(session.active_tab, 1, "and it is shown");
        let Tab::Markdown(md) = &session.tabs[1] else {
            panic!("not a markdown tab");
        };
        assert_eq!(md.source, "# two");
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
            resume_agent_command(&s, HarnessKind::Claude, "abc-123"),
            vec!["claude", "--resume", "abc-123"]
        );
    }

    #[test]
    fn new_command_pins_session_id_and_name() {
        let s = Settings::default();
        assert_eq!(
            new_agent_command(&s, HarnessKind::Claude, "abc-123", "my project"),
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
            resume_agent_command(&s, HarnessKind::Claude, "id1"),
            vec!["steam-run", "claude", "--resume", "id1"]
        );
        assert_eq!(
            new_agent_command(&s, HarnessKind::Claude, "id1", "proj"),
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
    fn kimi_command_builders_use_session_continue_and_bare_launch() {
        // Kimi: a learned id resumes with `--session <id>`; a fresh tab starts
        // bare; reopens `--continue` the directory's latest session. A custom
        // command (e.g. `steam-run kimi`) is honored via [harness.kimi].
        let s = Settings {
            harness: crate::settings::HarnessTable {
                kimi: Some(HarnessConfig {
                    enabled: Some(true),
                    command: Some(vec!["steam-run".into(), "kimi".into()]),
                }),
                ..Default::default()
            },
            ..Settings::default()
        };
        assert_eq!(
            resume_agent_command(&s, HarnessKind::Kimi, "session_abc"),
            vec!["steam-run", "kimi", "--session", "session_abc"]
        );
        assert_eq!(
            new_agent_command(&s, HarnessKind::Kimi, "", "proj"),
            vec!["steam-run", "kimi"]
        );
        assert_eq!(
            continue_agent_command(&s, HarnessKind::Kimi),
            vec!["steam-run", "kimi", "--continue"]
        );
    }

    #[test]
    fn spawn_command_resolves_per_harness_and_state() {
        let s = Settings::default();
        // Claude, no id → mint a UUID (session-id + name), flag assigned_new.
        let mut assigned = false;
        let mut t = ClaudeTab {
            name: "p".into(),
            harness: HarnessKind::Claude,
            session_id: None,
            fresh: false,
            id_reported: false,
            status_id: String::new(),
            status: crate::status::TabStatus::default(),
            pane: None,
            plugin_attach: PluginAttach::Idle,
            attach_when_shown: false,
        };
        let cmd = super::spawn_command_for_tab(&s, &mut t, &mut assigned);
        assert!(assigned && t.session_id.is_some());
        assert_eq!(cmd[..2], ["claude", "--session-id"]);

        // Kimi, fresh → bare new session, then `fresh` is cleared.
        let mut assigned = false;
        let mut t = ClaudeTab {
            name: "p".into(),
            harness: HarnessKind::Kimi,
            session_id: None,
            fresh: true,
            id_reported: false,
            status_id: String::new(),
            status: crate::status::TabStatus::default(),
            pane: None,
            plugin_attach: PluginAttach::Idle,
            attach_when_shown: false,
        };
        assert_eq!(
            super::spawn_command_for_tab(&s, &mut t, &mut assigned),
            vec!["kimi"]
        );
        assert!(!t.fresh && !assigned);
        // A subsequent (non-fresh) spawn continues the session.
        assert_eq!(
            super::spawn_command_for_tab(&s, &mut t, &mut assigned),
            vec!["kimi", "--continue"]
        );

        // Kimi with a learned id resumes it precisely.
        let mut t = ClaudeTab {
            name: "p".into(),
            harness: HarnessKind::Kimi,
            session_id: Some("session_z".into()),
            fresh: false,
            id_reported: false,
            status_id: String::new(),
            status: crate::status::TabStatus::default(),
            pane: None,
            plugin_attach: PluginAttach::Idle,
            attach_when_shown: false,
        };
        assert_eq!(
            super::spawn_command_for_tab(&s, &mut t, &mut assigned),
            vec!["kimi", "--session", "session_z"]
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
            harness: HarnessKind::Claude,
            session_id: None,
            fresh: false,
            id_reported: false,
            status_id: String::new(),
            status: crate::status::TabStatus::default(),
            pane: None,
            plugin_attach: PluginAttach::Idle,
            attach_when_shown: false,
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
            session_id: None,
            source: None,
        });
        app.handle_status_update(StatusUpdate {
            tab: "ta".into(),
            kind: StatusKind::SubagentStart,
            session_id: None,
            source: None,
        });
        // An unknown id must be a harmless no-op.
        app.handle_status_update(StatusUpdate {
            tab: "ghost".into(),
            kind: StatusKind::Busy,
            session_id: None,
            source: None,
        });

        let s = app.active_session().unwrap();
        let a = s.tabs[0].as_claude().unwrap();
        let b = s.tabs[2].as_claude().unwrap();
        assert_eq!(a.status.event, None);
        assert_eq!(a.status.subagents, 1);
        assert_eq!(b.status.event, Some(HookEvent::Waiting));
        assert_eq!(b.status.subagents, 0);
    }

    /// A pushed session id that differs from the tab's is adopted for every
    /// harness: a Kimi tab learns its id, and a Claude tab follows a `/clear` or
    /// `/resume`. A push matching the tab's current id changes nothing.
    #[test]
    fn status_update_adopts_changed_session_ids() {
        use crate::ipc::StatusUpdate;
        use crate::status::StatusKind;

        let mut app = app_with_tabs(vec![claude_tab("c"), claude_tab("k")]);
        {
            let s = app.sessions.get_mut("p").unwrap();
            if let Tab::Claude(c) = &mut s.tabs[0] {
                c.status_id = "tc".into();
                c.session_id = Some("claude-uuid".into());
            }
            if let Tab::Claude(k) = &mut s.tabs[1] {
                k.status_id = "tk".into();
                k.harness = HarnessKind::Kimi;
            }
        }
        let push = |app: &mut App, tab: &str, id: &str, source: Option<&str>| {
            app.handle_status_update(StatusUpdate {
                tab: tab.into(),
                kind: StatusKind::Session,
                session_id: Some(id.into()),
                source: source.map(str::to_string),
            });
        };
        let tab = |app: &App, i: usize| {
            let t = app.active_session().unwrap().tabs[i].as_claude().unwrap();
            (t.name.clone(), t.session_id.clone())
        };

        // Kimi learns its id without being renamed.
        push(&mut app, "tk", "session_xyz", None);
        assert_eq!(tab(&app, 1), ("k".into(), Some("session_xyz".into())));

        // Claude's launch report matches the minted id: a no-op.
        push(&mut app, "tc", "claude-uuid", Some("startup"));
        assert_eq!(tab(&app, 0), ("c".into(), Some("claude-uuid".into())));

        // `/clear` → a new conversation: adopted and given a fresh suffix.
        push(&mut app, "tc", "cleared", Some("clear"));
        assert_eq!(tab(&app, 0), ("c (2)".into(), Some("cleared".into())));

        // Compaction under a new id is the same conversation: name kept.
        push(&mut app, "tc", "compacted", Some("compact"));
        assert_eq!(tab(&app, 0), ("c (2)".into(), Some("compacted".into())));
    }

    /// The first id a freshly spawned tab reports is its launch: even when it
    /// differs from the id wrk spawned with, it's the same conversation.
    #[test]
    fn first_session_report_after_spawn_keeps_the_name() {
        use crate::ipc::StatusUpdate;
        use crate::status::StatusKind;

        let mut app = app_with_tabs(vec![claude_tab("c")]);
        if let Tab::Claude(c) = &mut app.sessions.get_mut("p").unwrap().tabs[0] {
            c.status_id = "tc".into();
            c.session_id = Some("spawned".into());
        }
        app.handle_status_update(StatusUpdate {
            tab: "tc".into(),
            kind: StatusKind::Session,
            session_id: Some("forked".into()),
            source: Some("resume".into()),
        });
        let t = app.active_session().unwrap().tabs[0].as_claude().unwrap();
        assert_eq!(t.name, "c");
        assert_eq!(t.session_id.as_deref(), Some("forked"));
    }

    #[test]
    fn switched_tab_name_prefers_known_names_then_suffixes() {
        let names = BTreeMap::from([
            ("a".to_string(), "proj".to_string()),
            ("b".to_string(), "proj (2)".to_string()),
        ]);
        // A session wrk already named keeps that name, continued or not.
        assert_eq!(
            super::switched_tab_name("proj (2)", "a", &names, false),
            Some("proj".into())
        );
        assert_eq!(
            super::switched_tab_name("x", "a", &names, true),
            Some("proj".into())
        );
        // An unknown, continued conversation keeps the tab's name.
        assert_eq!(super::switched_tab_name("proj", "new", &names, true), None);
        // An unknown new conversation takes the lowest free suffix, built from
        // the base name rather than stacking suffixes.
        assert_eq!(
            super::switched_tab_name("proj (2)", "new", &names, false),
            Some("proj (3)".into())
        );
        assert_eq!(
            super::next_session_name("notes (draft)", &names),
            "notes (draft) (2)"
        );
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

    /// #95: a target git would read as an option never reaches git, even when
    /// it arrives straight over the socket (the CLI's own check is bypassed).
    #[test]
    fn a_review_target_that_looks_like_an_option_is_refused() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // no git here; nothing would run anyway
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let ok = std::process::Command::new("git")
            .current_dir(repo)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success();
        assert!(ok, "git init");
        let mut app = empty_app();
        app.store.projects.push(Project {
            path: repo.to_path_buf(),
            ..bare_project("p", &[])
        });
        let written = repo.join("written-by-git");
        app.handle_review_request(ipc::ReviewRequest {
            tab: None,
            project: Some("p".into()),
            cwd: Some(repo.to_string_lossy().into_owned()),
            kind: ipc::ReviewKind::Start {
                target: Some(format!("--output={}", written.display())),
            },
        });
        assert!(app.review.is_none(), "no review opened");
        assert!(!written.exists(), "git never ran with the option");
        let err = app.error.as_deref().unwrap_or_default();
        assert!(err.contains("can't start with '-'"), "{err}");
    }

    #[test]
    fn review_mirror_key_is_pid_prefixed() {
        use crate::review::{ReviewSession, diff::DiffTarget};
        let s = ReviewSession::new(
            "proj".into(),
            Some("tab0".into()),
            DiffTarget::WorkingVsHead,
            vec![],
        );
        let key = crate::review_mirror_key(&s);
        // "<pid>-tab0" — the pid prefix keeps concurrent instances (and dead
        // predecessors, whose "tab0" repeats) from sharing a mirror file.
        assert_eq!(key, format!("{}-tab0", std::process::id()));
    }

    #[test]
    fn close_active_tab_keeps_last_claude() {
        // Closing the only Claude tab is a no-op (a project keeps ≥1 session).
        let mut app = app_with_tabs(vec![claude_tab("c")]);
        app.close_active_tab();
        assert_eq!(app.active_session().unwrap().tabs.len(), 1);
    }

    fn bare_project(name: &str, sessions: &[&str]) -> Project {
        Project {
            name: name.to_string(),
            path: PathBuf::from("/tmp"),
            tags: vec![],
            layout_mode: None,
            shell_passthrough: None,
            claude_sessions: sessions
                .iter()
                .map(|n| crate::store::SessionRef {
                    name: n.to_string(),
                    session_id: None,
                    harness: HarnessKind::Claude,
                })
                .collect(),
            session_names: BTreeMap::new(),
            plugin: None,
            plugin_config: BTreeMap::new(),
            remote: None,
        }
    }

    /// `boxes` lists box `dev-box` with sessions `ids`.
    fn listed(app: &mut App, ids: &[&str]) {
        use wrk_plugin::proto::{PluginSession, ProjectsResponse, RemoteProject};
        let sessions = ids
            .iter()
            .map(|id| PluginSession {
                id: id.to_string(),
                name: Some(id.replace('/', "-")),
                state: Default::default(),
                status: None,
            })
            .collect();
        let listing = plugin::Listing {
            projects: ProjectsResponse {
                group: Some("boxes".into()),
                projects: vec![RemoteProject {
                    id: "i-1".into(),
                    name: "dev-box".into(),
                    sessions,
                    ..Default::default()
                }],
            },
            can_create: true,
            can_remove: true,
        };
        app.apply_listing("boxes", Ok(Some(listing)), Rect::new(0, 0, 120, 40));
    }

    fn tab_names(app: &App, project: &str) -> Vec<String> {
        app.sessions[project]
            .tabs
            .iter()
            .map(|t| t.name().to_string())
            .collect()
    }

    #[test]
    fn a_plugin_listing_shows_as_its_own_sidebar_group() {
        use crate::ui::projects::{RowKey, SidebarRow};
        let mut app = empty_app();
        app.store.projects.push(bare_project("wrk", &[]));
        app.refresh_sidebar();
        assert_eq!(
            app.sidebar
                .rows
                .iter()
                .map(SidebarRow::key)
                .collect::<Vec<_>>(),
            vec![RowKey::Project("wrk".into())],
            "no headings without plugin groups"
        );
        listed(&mut app, &["web/main", "web/fix"]);
        app.refresh_sidebar();
        assert_eq!(
            app.sidebar
                .rows
                .iter()
                .map(SidebarRow::key)
                .collect::<Vec<_>>(),
            vec![
                RowKey::Group("".into()),
                RowKey::Project("wrk".into()),
                RowKey::Group("boxes".into()),
                RowKey::Project("boxes/i-1".into()),
                RowKey::Session("boxes/i-1".into(), 0),
                RowKey::Session("boxes/i-1".into(), 1),
            ]
        );
        assert_eq!(app.sidebar.rows[3].label, "dev-box");
        // Never in projects.toml's store.
        assert!(app.store.find("boxes/i-1").is_none());
        assert_eq!(app.project("boxes/i-1").unwrap().display_name(), "dev-box");
    }

    #[test]
    fn opening_a_listed_project_mirrors_its_sessions_and_attaches_only_the_shown_one() {
        let mut app = empty_app();
        listed(&mut app, &["web/main", "web/fix", "api/main"]);
        app.open_project("boxes/i-1", Some(1), Rect::new(0, 0, 120, 40));
        assert_eq!(
            tab_names(&app, "boxes/i-1"),
            ["web-main", "web-fix", "api-main"]
        );
        let session = &app.sessions["boxes/i-1"];
        assert_eq!(session.active_tab, 1);
        let states: Vec<(PluginAttach, bool)> = session
            .claude_tabs()
            .map(|t| (t.plugin_attach.clone(), t.attach_when_shown))
            .collect();
        assert_eq!(
            states,
            [
                (PluginAttach::Idle, true),
                (PluginAttach::Pending, false),
                (PluginAttach::Idle, true)
            ]
        );
    }

    #[test]
    fn relisting_reconciles_open_tabs_and_keeps_the_shown_one() {
        let mut app = empty_app();
        listed(&mut app, &["web/main", "web/fix"]);
        app.open_project("boxes/i-1", Some(1), Rect::new(0, 0, 120, 40));
        let shown = app.sessions["boxes/i-1"]
            .active_claude_tab()
            .unwrap()
            .status_id
            .clone();
        // A session was made before `fix`, and `main` went away.
        listed(&mut app, &["web/new", "web/fix"]);
        assert_eq!(tab_names(&app, "boxes/i-1"), ["web-new", "web-fix"]);
        let session = &app.sessions["boxes/i-1"];
        assert_eq!(session.active_tab, 1);
        assert_eq!(
            session.active_claude_tab().unwrap().status_id,
            shown,
            "same tab"
        );
        // The box itself goes: its session goes with it.
        app.apply_listing(
            "boxes",
            Ok(Some(plugin::Listing {
                projects: Default::default(),
                can_create: true,
                can_remove: false,
            })),
            Rect::new(0, 0, 120, 40),
        );
        assert!(!app.sessions.contains_key("boxes/i-1"));
        assert_eq!(app.active_project_name, None);
    }

    #[test]
    fn closing_a_listed_tab_only_detaches_it() {
        let mut app = empty_app();
        listed(&mut app, &["web/main", "web/fix"]);
        app.open_project("boxes/i-1", Some(0), Rect::new(0, 0, 120, 40));
        app.close_active_tab();
        assert_eq!(tab_names(&app, "boxes/i-1"), ["web-main", "web-fix"]);
        let tab = app.sessions["boxes/i-1"].active_claude_tab().unwrap();
        assert!(tab.pane.is_none() && !tab.attach_when_shown);
        assert_eq!(tab.plugin_attach, PluginAttach::Idle);
    }

    /// Send `request` as if over the socket; the other end of its connection.
    fn api_call(app: &mut App, request: ipc::Request) -> UnixStream {
        let (client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        app.handle_incoming(
            ipc::Incoming {
                request,
                reply: ipc::Reply::new(server),
            },
            Rect::new(0, 0, 120, 40),
        );
        client
    }

    fn reply_of(client: UnixStream) -> serde_json::Value {
        use std::io::BufRead;
        let mut line = String::new();
        std::io::BufReader::new(client)
            .read_line(&mut line)
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn create_request(plugin: &str, project: Option<&str>) -> ipc::Request {
        ipc::Request::Create(ipc::CreateRequest {
            plugin: plugin.into(),
            project: project.map(Into::into),
            values: [("name".to_string(), "api".to_string())].into(),
            open: true,
        })
    }

    #[test]
    fn an_api_create_is_answered_once_its_plugin_has_and_opens_what_it_made() {
        let mut app = empty_app();
        app.plugin_names = vec!["boxes".into()];
        listed(&mut app, &["web/main"]);
        let client = api_call(&mut app, create_request("boxes", Some("boxes/i-1")));
        assert_eq!(app.api_creates.len(), 1, "waiting on the plugin");
        let ticket = app.api_creates[0].ticket;
        // Not answered yet.
        client.set_nonblocking(true).unwrap();
        assert!(std::io::Read::read(&mut &client, &mut [0u8; 8]).is_err());
        client.set_nonblocking(false).unwrap();

        // A result for some other create (the form's has no ticket) isn't this one's.
        app.finish_api_create("boxes", ticket + 1, Err(anyhow::anyhow!("not mine")));
        assert_eq!(app.api_creates.len(), 1);
        app.finish_api_create(
            "boxes",
            ticket,
            Ok(plugin::CreateResponse {
                project: Some("i-1".into()),
                session: Some("api/main".into()),
                message: Some("creating api".into()),
            }),
        );
        assert!(app.api_creates.is_empty());
        let reply = reply_of(client);
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["project"], "boxes/i-1");
        assert_eq!(reply["session"], "api/main");
        assert_eq!(reply["message"], "creating api");
        assert_eq!(app.info.as_deref(), Some("creating api"));
        // It opens once a listing includes it.
        listed(&mut app, &["web/main", "api/main"]);
        assert_eq!(app.active_project_name.as_deref(), Some("boxes/i-1"));
        assert_eq!(app.sessions["boxes/i-1"].active_tab, 1);
    }

    #[test]
    fn an_api_create_reports_what_went_wrong() {
        let mut app = empty_app();
        app.plugin_names = vec!["boxes".into()];
        listed(&mut app, &["web/main"]);
        let reply = reply_of(api_call(&mut app, create_request("nope", None)));
        assert_eq!(reply["ok"], false);
        assert!(
            reply["error"]
                .as_str()
                .unwrap()
                .contains("no plugin 'nope'")
        );
        let reply = reply_of(api_call(
            &mut app,
            create_request("boxes", Some("boxes/i-9")),
        ));
        assert!(
            reply["error"]
                .as_str()
                .unwrap()
                .contains("no project 'boxes/i-9'")
        );
        assert!(app.api_creates.is_empty());

        let client = api_call(&mut app, create_request("boxes", None));
        let ticket = app.api_creates[0].ticket;
        app.finish_api_create("boxes", ticket, Err(anyhow::anyhow!("name taken")));
        let reply = reply_of(client);
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"], "name taken");
        assert_eq!(app.active_project_name, None, "nothing to open");
    }

    #[test]
    fn a_create_opens_what_it_made_once_listed() {
        let mut app = empty_app();
        listed(&mut app, &["web/main"]);
        app.finish_plugin_create(
            "boxes",
            Ok(plugin::CreateResponse {
                project: Some("i-1".into()),
                session: Some("api/main".into()),
                message: Some("creating api".into()),
            }),
        );
        assert_eq!(app.info.as_deref(), Some("creating api"));
        assert!(!app.sessions.contains_key("boxes/i-1"), "not listed yet");
        listed(&mut app, &["web/main", "api/main"]);
        assert_eq!(app.active_project_name.as_deref(), Some("boxes/i-1"));
        assert_eq!(app.sessions["boxes/i-1"].active_tab, 1);
    }

    #[test]
    fn d_on_a_listed_project_or_tab_asks_its_plugin_how_to_remove_it() {
        use crate::ui::projects::RowKey;
        let body = Rect::new(0, 0, 120, 40);
        let mut app = empty_app();
        listed(&mut app, &["web/main", "web/fix"]);
        app.refresh_sidebar();
        let form = |app: &App| match app.modal.as_ref() {
            Some(ModalState::PluginForm(m)) => Some((
                m.purpose,
                m.context.clone(),
                m.session_id.clone(),
                m.form.is_some(),
            )),
            _ => None,
        };

        // A plugin that doesn't remove: say where to do it instead.
        app.remote.groups[0].can_remove = false;
        app.sidebar.select_key(&RowKey::Project("boxes/i-1".into()));
        handle_projects_key(&mut app, key('d'), body).unwrap();
        assert!(app.modal.is_none());
        assert!(app.info.as_deref().unwrap().contains("remove it there"));
        app.remote.groups[0].can_remove = true;

        // The project: a removal form for it, loading.
        handle_projects_key(&mut app, key('d'), body).unwrap();
        assert_eq!(
            form(&app),
            Some((
                plugin::FormFor::Remove,
                Some("boxes/i-1".into()),
                None,
                false
            ))
        );
        // A late creation form doesn't fill it; the removal form does.
        let described = |title: &str| {
            Ok(plugin::FormResponse {
                title: title.into(),
                ..Default::default()
            })
        };
        app.show_plugin_form("boxes", plugin::FormFor::Create, described("New"));
        assert!(!form(&app).unwrap().3);
        app.show_plugin_form(
            "boxes",
            plugin::FormFor::Remove,
            described("Terminate dev-box"),
        );
        assert!(form(&app).unwrap().3);

        // A tab: the form is for its session.
        app.modal = None;
        app.sidebar
            .select_key(&RowKey::Session("boxes/i-1".into(), 1));
        handle_projects_key(&mut app, key('d'), body).unwrap();
        assert_eq!(
            form(&app),
            Some((
                plugin::FormFor::Remove,
                Some("boxes/i-1".into()),
                Some("web/fix".into()),
                false
            ))
        );
    }

    #[test]
    fn a_removal_closes_its_form_or_says_why_it_failed_there() {
        let mut app = empty_app();
        listed(&mut app, &["web/main"]);
        let mut m =
            PluginFormModal::removing("boxes".into(), "boxes".into(), "boxes/i-1".into(), None);
        m.set_form(plugin::FormResponse::default());
        m.submitting = true;
        app.modal = Some(ModalState::PluginForm(m));
        app.finish_plugin_remove("boxes", Err(anyhow::anyhow!("type the box's name")));
        match app.modal.as_ref() {
            Some(ModalState::PluginForm(m)) => {
                assert_eq!(m.error.as_deref(), Some("type the box's name"));
                assert!(!m.submitting, "it can be sent again");
            }
            other => panic!("{other:?}"),
        }
        // A creation's result is not this form's.
        if let Some(ModalState::PluginForm(m)) = app.modal.as_mut() {
            m.submitting = true;
        }
        app.finish_plugin_create("boxes", Err(anyhow::anyhow!("unrelated")));
        assert!(
            matches!(&app.modal, Some(ModalState::PluginForm(m)) if m.error.as_deref() == Some("type the box's name"))
        );
        app.finish_plugin_remove(
            "boxes",
            Ok(plugin::RemoveResponse {
                message: Some("terminating dev-box".into()),
            }),
        );
        assert!(app.modal.is_none());
        assert_eq!(app.info.as_deref(), Some("terminating dev-box"));
    }

    #[test]
    fn sidebar_nests_live_and_persisted_sessions_and_follows_the_shown_tab() {
        use crate::ui::projects::{RowKey, SessionKind, SidebarRow};
        let mut app = app_with_tabs(vec![claude_tab("a"), md_tab("m"), claude_tab("b")]);
        app.store.projects.push(bare_project("p", &[]));
        // Not loaded: its persisted sessions are listed (and openable) anyway.
        app.store.projects.push(bare_project("q", &["x"]));
        app.sessions.get_mut("p").unwrap().active_tab = 2;
        app.focus = Focus::Claude;
        app.refresh_sidebar();

        let key = |p: &str, t| RowKey::Session(p.to_string(), t);
        let keys: Vec<_> = app.sidebar.rows.iter().map(SidebarRow::key).collect();
        assert_eq!(
            keys,
            vec![
                RowKey::Project("p".into()),
                key("p", 0),
                key("p", 1),
                key("p", 2),
                RowKey::Project("q".into()),
                key("q", 0),
            ]
        );
        let md = &app.sidebar.rows[2].session.as_ref().unwrap().1;
        assert_eq!(md.kind, SessionKind::Markdown);
        assert!(!app.sidebar.rows[5].loaded);
        // Outside the sidebar the cursor tracks the shown tab…
        assert_eq!(app.sidebar.selected_target(), Some(("p".into(), Some(2))));

        // …but while navigating the sidebar it stays where the user put it.
        app.focus = Focus::Projects;
        app.sidebar.select_next();
        app.refresh_sidebar();
        assert_eq!(
            app.sidebar.selected_row().map(SidebarRow::key),
            Some(RowKey::Project("q".into()))
        );
    }

    #[test]
    fn tab_strip_only_shows_while_the_sidebar_is_hidden() {
        let mut app = empty_app();
        assert!(!app.tab_strip_shown(3), "the sidebar lists the sessions");
        app.sidebar_hidden = true;
        assert!(app.tab_strip_shown(3));
        assert!(!app.tab_strip_shown(0));
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
            session_names: BTreeMap::new(),
            plugin: None,
            plugin_config: BTreeMap::new(),
            remote: None,
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

    /// The notification banner draws over the top row, and a click on it
    /// opens (here: reports the gone target) or, on its ✕, just dismisses.
    #[test]
    fn the_notification_banner_draws_on_top_and_takes_clicks() {
        use ratatui::{Terminal, backend::TestBackend};
        let dir = tempfile::tempdir().unwrap();
        let mut app = crate::api::tests::open_app(dir.path());
        let area = crate::Rect::new(0, 0, 120, 41);
        let notify = |app: &mut crate::App, title: &str, project: Option<&str>| {
            app.notifications.push(
                crate::ipc::BannerRequest {
                    title: title.into(),
                    body: Some("ready to look at".into()),
                    session: None,
                    project: project.map(String::from),
                    widget: None,
                    ttl: None,
                },
                std::time::Instant::now(),
            );
        };
        notify(&mut app, "first", None);
        notify(&mut app, "Build done", Some("gone"));

        let mut term = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer();
        let top: String = (0..area.width).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(top.contains("Build done — ready to look at"), "{top}");
        assert!(top.contains("+1 · Alt+o open · Alt+c clear ✕"), "{top}");

        let click = |x: u16| crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: x,
            row: 0,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        // Clicking the message opens it: its project is gone, so that's reported.
        crate::handle_mouse(&mut app, click(10), area);
        assert!(app.error.as_deref().unwrap().contains("no project 'gone'"));
        assert_eq!(app.notifications.newest().unwrap().title, "first");
        // The ✕ dismisses without opening.
        app.error = None;
        crate::handle_mouse(&mut app, click(area.width - 2), area);
        assert!(app.notifications.is_empty());
        assert!(app.error.is_none());
    }

    /// What a program in any pane copies (OSC 52) is passed on to the host
    /// terminal's clipboard: a background tab's and the shell's as well as
    /// the shown tab's.
    #[test]
    fn what_programs_in_panes_copy_goes_to_the_host_clipboard() {
        use std::time::{Duration, Instant};
        let dir = tempfile::tempdir().unwrap();
        let mut app = crate::api::tests::open_app(dir.path());
        let copying = |b64: &str| {
            let script = format!("printf '\\033]52;c;{b64}\\007'; exec cat");
            let cmd = ["sh".to_string(), "-c".into(), script];
            crate::PtyPane::spawn(&cmd, dir.path(), 24, 80, &[]).unwrap()
        };
        let session = app.sessions.get_mut("p").unwrap();
        // "side" is a background tab; "main" is shown.
        session.tabs[1].as_claude_mut().unwrap().pane = Some(copying("c2lkZQ=="));
        session.shell = Some(copying("c2hlbGw="));

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut copied = Vec::new();
        while copied.len() < 2 && Instant::now() < deadline {
            copied.extend(app.take_copied().into_iter().map(|c| c.text));
            std::thread::sleep(Duration::from_millis(20));
        }
        copied.sort();
        assert_eq!(copied, ["shell", "side"]);
        assert!(app.take_copied().is_empty());

        // As the host terminal gets it.
        assert_eq!(crate::osc52("side", false), "\x1b]52;c;c2lkZQ==\x07");
        assert_eq!(crate::osc52("side", true), "\x1b]52;p;c2lkZQ==\x07");
    }

    #[test]
    fn a_click_outside_the_widget_hides_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = crate::api::tests::open_app(dir.path());
        app.settings.widget.insert(
            "w".into(),
            crate::settings::WidgetConfig {
                command: vec!["sh".into(), "-c".into(), "exec cat".into()],
                ..Default::default()
            },
        );
        let area = crate::Rect::new(0, 0, 120, 41);
        app.show_widget("w", crate::body_rect(area)).unwrap();
        let click = |x: u16, y: u16| crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: x,
            row: y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let inner = crate::widget::overlay_inner(crate::body_rect(area));
        crate::handle_mouse(&mut app, click(inner.x + 2, inner.y + 2), area);
        assert!(app.widgets.shown.is_some(), "a click inside stays");
        crate::handle_mouse(&mut app, click(1, 20), area);
        assert!(app.widgets.shown.is_none());
        assert_eq!(app.widgets.instances.len(), 1, "still running");
    }
}
