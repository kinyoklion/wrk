//! The API widgets and scripts use to drive wrk: list the sessions, type into
//! one, show one, post and dismiss notifications, and show or hide widgets.
//! Callers reach it through `wrk api …` (the CLI below) or by writing the
//! request JSON to `WRK_SOCK` themselves (see `ipc` and `docs/widgets.md`).
//! This module is wrk's side: the [`App`] handlers, run on the UI thread by
//! the event loop, plus the CLI client.

use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use clap::Subcommand;
use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::ipc::{self, ApiReply, Request};
use crate::notifications::NotifyTarget;
use crate::pane::terminal::PtyPane;
use crate::ui::projects::ProjectStatus;
use crate::{App, Focus, Tab, push_error};

/// How long after the text `send --submit` presses Enter. An agent TUI
/// (Claude Code among them) can take an Enter that arrives in the same read
/// as a paste for part of the paste; after a short pause it's a keypress.
const SUBMIT_DELAY: Duration = Duration::from_millis(150);

/// One session, as `sessions` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEntry {
    /// What `send`, `focus` and `banner --session` take. An agent tab's id is
    /// its `WRK_TAB`; a project's shell is `shell:<project>`; a markdown tab is
    /// `md:<project>:<index>`.
    pub id: String,
    /// `agent`, `shell`, or `markdown`.
    pub kind: String,
    /// The agent's harness (`claude`, `kimi`). Agents only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    pub project: String,
    /// The project's directory.
    pub project_path: String,
    pub name: String,
    /// Agents only: `busy`, `idle` (done, waiting for a prompt), `attention`
    /// (needs the user, e.g. a permission prompt), or `none` (not running).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Has a live terminal, so `send` can type into it.
    pub running: bool,
    /// In the project on screen, and its shown tab (or its shell).
    pub active: bool,
    /// Has the keyboard (once any widget is out of the way).
    pub focused: bool,
    /// The agent's own session id (e.g. Claude's), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// A markdown tab's file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// What a session id points at.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Tab { project: String, index: usize },
    Shell { project: String },
}

fn status_name(status: ProjectStatus) -> &'static str {
    match status {
        ProjectStatus::None => "none",
        ProjectStatus::Busy => "busy",
        ProjectStatus::Waiting => "idle",
        ProjectStatus::Attention => "attention",
    }
}

impl App {
    /// Route a request drained from the socket: answer an API request, or hand
    /// any other (`wrk view` / `hook` / `review` / `confirm` / `notify`) to
    /// `handle_request` along with its connection.
    pub(crate) fn handle_incoming(&mut self, incoming: ipc::Incoming, body: Rect) {
        if !incoming.request.is_api() {
            self.handle_request(incoming.request, incoming.reply);
            return;
        }
        let answer = self.handle_api(incoming.request.clone(), body);
        incoming.respond(answer);
    }

    fn handle_api(&mut self, request: Request, body: Rect) -> ApiReply {
        match request {
            Request::Sessions => Ok(json!({ "sessions": self.list_sessions() })),
            Request::Send(req) => {
                let target = self.resolve_session(&req.session)?;
                self.type_into(&target, &req.text)?;
                if req.submit {
                    self.pending_submits
                        .push((Instant::now() + SUBMIT_DELAY, req.session));
                }
                if req.focus {
                    self.focus_target(&target, body);
                }
                Ok(json!({}))
            }
            Request::Focus(req) => {
                let target = self.resolve_session(&req.session)?;
                self.focus_target(&target, body);
                Ok(json!({}))
            }
            Request::Banner(req) => {
                if req.title.trim().is_empty() {
                    return Err("a notification needs a title".into());
                }
                let id = self.notifications.push(req, Instant::now());
                Ok(json!({ "id": id }))
            }
            Request::Dismiss(req) => {
                if self.notifications.dismiss(req.id) {
                    Ok(json!({}))
                } else {
                    Err(format!("no notification {}", req.id))
                }
            }
            Request::Show(req) => {
                self.show_widget(&req.widget, body)?;
                Ok(json!({}))
            }
            Request::Hide => {
                self.hide_widget();
                Ok(json!({}))
            }
            // `handle_incoming` only routes API requests here.
            other => Err(format!("not an API request: {other:?}")),
        }
    }

    /// Every session of every open project, in sidebar order: each project's
    /// tabs, then its shell.
    pub(crate) fn list_sessions(&self) -> Vec<SessionEntry> {
        let projects = self
            .store
            .projects
            .iter()
            .chain(self.remote.groups.iter().flat_map(|g| g.projects.iter()));
        let mut out = Vec::new();
        for project in projects {
            let Some(session) = self.sessions.get(&project.name) else {
                continue;
            };
            let on_screen = self.active_project_name.as_deref() == Some(project.name.as_str());
            let project_path = project.path.to_string_lossy().into_owned();
            let entry = |id: String, kind: &str, name: &str| SessionEntry {
                id,
                kind: kind.to_string(),
                harness: None,
                project: project.name.clone(),
                project_path: project_path.clone(),
                name: name.to_string(),
                status: None,
                running: false,
                active: false,
                focused: false,
                session_id: None,
                path: None,
            };
            for (index, tab) in session.tabs.iter().enumerate() {
                let active = on_screen && index == session.active_tab;
                let focused = active && self.focus == Focus::Claude;
                out.push(match tab {
                    Tab::Claude(t) => SessionEntry {
                        harness: Some(t.harness.id().to_string()),
                        status: Some(status_name(crate::ui::tab_status(t)).to_string()),
                        running: t.pane.is_some(),
                        active,
                        focused,
                        session_id: t.session_id.clone(),
                        ..entry(t.status_id.clone(), "agent", &t.name)
                    },
                    Tab::Markdown(m) => SessionEntry {
                        active,
                        focused,
                        path: Some(m.path.to_string_lossy().into_owned()),
                        ..entry(format!("md:{}:{index}", project.name), "markdown", &m.name)
                    },
                });
            }
            out.push(SessionEntry {
                running: session.shell.is_some(),
                active: on_screen,
                focused: on_screen && self.focus == Focus::Shell,
                ..entry(format!("shell:{}", project.name), "shell", "shell")
            });
        }
        out
    }

    fn resolve_session(&self, id: &str) -> Result<Target, String> {
        let missing = || format!("no session '{id}' (see `wrk api sessions`)");
        if let Some(project) = id.strip_prefix("shell:") {
            return self
                .sessions
                .contains_key(project)
                .then(|| Target::Shell {
                    project: project.to_string(),
                })
                .ok_or_else(missing);
        }
        if let Some((project, index)) = id.strip_prefix("md:").and_then(|r| r.rsplit_once(':')) {
            let index: usize = index.parse().map_err(|_| missing())?;
            return match self.sessions.get(project).and_then(|s| s.tabs.get(index)) {
                Some(Tab::Markdown(_)) => Ok(Target::Tab {
                    project: project.to_string(),
                    index,
                }),
                _ => Err(missing()),
            };
        }
        self.sessions
            .iter()
            .find_map(|(project, session)| {
                let index = session
                    .tabs
                    .iter()
                    .position(|t| t.as_claude().is_some_and(|c| c.status_id == id))?;
                Some(Target::Tab {
                    project: project.clone(),
                    index,
                })
            })
            .ok_or_else(missing)
    }

    fn target_pane_mut(&mut self, target: &Target) -> Result<&mut PtyPane, String> {
        let not_running = || "that session isn't running".to_string();
        match target {
            Target::Tab { project, index } => {
                let tab = self
                    .sessions
                    .get_mut(project)
                    .and_then(|s| s.tabs.get_mut(*index))
                    .ok_or_else(not_running)?;
                match tab {
                    Tab::Claude(t) => t.pane.as_mut().ok_or_else(not_running),
                    Tab::Markdown(_) => Err("a markdown tab can't take text".into()),
                }
            }
            Target::Shell { project } => self
                .sessions
                .get_mut(project)
                .and_then(|s| s.shell.as_mut())
                .ok_or_else(not_running),
        }
    }

    /// Type `text` into a session as a paste (bracketed when the program asked
    /// for that), so a multi-line text arrives whole instead of submitting line
    /// by line.
    fn type_into(&mut self, target: &Target, text: &str) -> Result<(), String> {
        let pane = self.target_pane_mut(target)?;
        let bytes = crate::paste_bytes(pane, &crate::normalize_paste(text));
        pane.write(&bytes)
            .map_err(|e| format!("writing to the session: {e:#}"))?;
        pane.scroll_to_bottom();
        Ok(())
    }

    /// Press Enter for the `send --submit` requests whose pause is over.
    pub(crate) fn flush_pending_submits(&mut self, now: Instant) {
        if self.pending_submits.is_empty() {
            return;
        }
        let (due, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending_submits)
            .into_iter()
            .partition(|(at, _)| *at <= now);
        self.pending_submits = waiting;
        for (_, id) in due {
            if let Ok(target) = self.resolve_session(&id)
                && let Ok(pane) = self.target_pane_mut(&target)
            {
                let _ = pane.write(b"\r");
            }
        }
    }

    /// Show a session: open its project on that tab (or its shell), focus it,
    /// and get any widget out of the way.
    fn focus_target(&mut self, target: &Target, body: Rect) {
        self.hide_widget();
        match target {
            Target::Tab { project, index } => {
                self.open_project(project, Some(*index), body);
                self.focus = Focus::Claude;
            }
            Target::Shell { project } => {
                self.open_project(project, None, body);
                self.focus = Focus::Shell;
            }
        }
    }

    /// Act on the newest notification (the `OpenNotification` shortcut, or a
    /// click on the banner): show what it points at and take it down. Returns
    /// false when there is none.
    pub(crate) fn open_notification(&mut self, body: Rect) -> bool {
        let Some(n) = self.notifications.pop_newest() else {
            return false;
        };
        let result = match &n.target {
            NotifyTarget::Session(id) => self
                .resolve_session(id)
                .map(|target| self.focus_target(&target, body)),
            NotifyTarget::Project(p) if self.project(p).is_some() => {
                self.hide_widget();
                self.open_project(p, None, body);
                self.focus = Focus::Claude;
                Ok(())
            }
            NotifyTarget::Project(p) => Err(format!("no project '{p}'")),
            NotifyTarget::Widget(w) => self.show_widget(w, body),
            NotifyTarget::None => Ok(()),
        };
        if let Err(e) = result {
            push_error(&mut self.error, format!("notification: {e}"));
        }
        true
    }
}

/// `wrk api` subcommands: the CLI over the API, for widgets and scripts
/// running inside wrk (they need `WRK_SOCK`, which wrk exports to every pane
/// and widget).
#[derive(Subcommand)]
pub enum ApiAction {
    /// List the open projects' sessions, one per line:
    /// `id<TAB>kind<TAB>project<TAB>name<TAB>status` (status is `-` except for
    /// agents). `--json` prints every field.
    Sessions {
        #[arg(long)]
        json: bool,
    },
    /// Type text into a session, as if pasted. The text is the remaining
    /// arguments joined by spaces, or stdin when there are none.
    Send {
        /// Session id, from `wrk api sessions`.
        session: String,
        text: Vec<String>,
        /// Press Enter after the text.
        #[arg(long)]
        submit: bool,
        /// Also show the session (and hide the widget on screen).
        #[arg(long)]
        focus: bool,
    },
    /// Show a session: open its project on it and focus it.
    Focus { session: String },
    /// Post a notification banner; prints its id. Activating it (click, or
    /// the open-notification key) shows its target. (For a one-line
    /// status-bar message, use `wrk notify`.)
    Banner {
        title: String,
        #[arg(long)]
        body: Option<String>,
        /// Open this session when activated.
        #[arg(long, conflicts_with_all = ["project", "widget"])]
        session: Option<String>,
        /// Open this project when activated.
        #[arg(long, conflicts_with = "widget")]
        project: Option<String>,
        /// Show this widget when activated.
        #[arg(long)]
        widget: Option<String>,
        /// Take it down after this many seconds.
        #[arg(long, value_name = "SECS")]
        ttl: Option<u64>,
    },
    /// Dismiss a notification banner by the id `banner` printed.
    Dismiss { id: u64 },
    /// Show a widget, starting it if needed.
    Show { widget: String },
    /// Hide the widget on screen (it keeps running).
    Hide,
}

pub fn cmd_api(action: ApiAction) -> Result<()> {
    let sock = std::env::var_os("WRK_SOCK").ok_or_else(|| {
        anyhow!("`wrk api` must run inside wrk (a pane or a widget): WRK_SOCK is not set")
    })?;
    let sock = Path::new(&sock);
    let call = |req: Request| ipc::call(sock, &req);
    match action {
        ApiAction::Sessions { json } => {
            let body = call(Request::Sessions)?;
            let sessions: Vec<SessionEntry> =
                serde_json::from_value(body.get("sessions").cloned().unwrap_or_default())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&sessions)?);
            } else {
                for s in sessions {
                    println!("{}", tsv_row(&s));
                }
            }
        }
        ApiAction::Send {
            session,
            text,
            submit,
            focus,
        } => {
            let text = if text.is_empty() {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s)?;
                // `echo …` and heredocs end in a newline the user didn't mean
                // as part of the text.
                s.strip_suffix('\n')
                    .map(|t| t.strip_suffix('\r').unwrap_or(t))
                    .unwrap_or(&s)
                    .to_string()
            } else {
                text.join(" ")
            };
            call(Request::Send(ipc::SendRequest {
                session,
                text,
                submit,
                focus,
            }))?;
        }
        ApiAction::Focus { session } => {
            call(Request::Focus(ipc::FocusRequest { session }))?;
        }
        ApiAction::Banner {
            title,
            body,
            session,
            project,
            widget,
            ttl,
        } => {
            let reply = call(Request::Banner(ipc::BannerRequest {
                title,
                body,
                session,
                project,
                widget,
                ttl,
            }))?;
            if let Some(id) = reply.get("id") {
                println!("{id}");
            }
        }
        ApiAction::Dismiss { id } => {
            call(Request::Dismiss(ipc::DismissRequest { id }))?;
        }
        ApiAction::Show { widget } => {
            call(Request::Show(ipc::ShowRequest { widget }))?;
        }
        ApiAction::Hide => {
            call(Request::Hide)?;
        }
    }
    Ok(())
}

/// A `sessions` line: tab-separated, with tabs and newlines in names flattened
/// so every session stays one parseable line.
fn tsv_row(s: &SessionEntry) -> String {
    let field = |v: &str| v.replace(['\t', '\n', '\r'], " ");
    [
        field(&s.id),
        field(&s.kind),
        field(&s.project),
        field(&s.name),
        field(s.status.as_deref().unwrap_or("-")),
    ]
    .join("\t")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ipc::{BannerRequest, FocusRequest, SendRequest};
    use crate::settings::{HarnessKind, Settings};
    use crate::store::{Project, ProjectStore};
    use crate::{ClaudeTab, MarkdownTab, PluginAttach, ProjectSession};
    use ratatui::buffer::Buffer;
    use ratatui::widgets::Widget;
    use std::path::{Path, PathBuf};

    pub const BODY: Rect = Rect::new(0, 0, 120, 40);

    /// `cat` in a PTY: echoes what's typed, and prints each submitted line.
    pub fn cat_pane(dir: &Path) -> PtyPane {
        let cmd = ["sh", "-c", "exec cat"].map(String::from);
        PtyPane::spawn(&cmd, dir, 24, 80, &[]).unwrap()
    }

    /// What `pane` shows, row by row.
    pub fn screen(pane: &PtyPane) -> String {
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        crate::pane::terminal::PtyPaneWidget(pane).render(area, &mut buf);
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Wait (up to a few seconds) for `cond` on `pane`'s screen.
    pub fn wait_for(pane: &PtyPane, cond: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let text = screen(pane);
            if cond(&text) || Instant::now() > deadline {
                return text;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn agent_tab(name: &str, status_id: &str, dir: &Path) -> Tab {
        Tab::Claude(ClaudeTab {
            name: name.into(),
            harness: HarnessKind::Claude,
            // A known id: reopening never mints (and so never persists) one.
            session_id: Some(format!("sess-{name}")),
            fresh: false,
            id_reported: false,
            status_id: status_id.into(),
            status: crate::status::TabStatus::default(),
            pane: Some(cat_pane(dir)),
            plugin_attach: PluginAttach::Idle,
            attach_when_shown: false,
        })
    }

    fn md_tab(dir: &Path) -> Tab {
        Tab::Markdown(MarkdownTab {
            name: "notes.md".into(),
            path: dir.join("notes.md"),
            source: "# hi".into(),
            rendered: wrk_markdown::RenderedDoc::default(),
            render_width: 0,
            theme: wrk_markdown::MdTheme::default(),
            diagram_ctx: wrk_markdown::DiagramCtx::default(),
            heading_images: false,
            cell_size: (8, 16),
            state: wrk_markdown::MarkdownViewState::new(),
        })
    }

    pub fn project(name: &str, path: &Path) -> Project {
        Project {
            name: name.into(),
            path: PathBuf::from(path),
            tags: vec![],
            layout_mode: None,
            shell_passthrough: None,
            claude_sessions: vec![],
            session_names: Default::default(),
            plugin: None,
            plugin_config: Default::default(),
            remote: None,
        }
    }

    /// Project `p` open with two agent tabs (`t-main`, `t-side`), a markdown
    /// tab, and a shell, every terminal a live `cat`. Opening it again spawns
    /// and persists nothing (every pane is live, every tab has an id), and the
    /// harness/shell commands are `cat` regardless, so no test runs a real
    /// agent or touches the user's projects.toml.
    pub fn open_app(dir: &Path) -> App {
        let settings = Settings {
            claude_command: ["sh", "-c", "exec cat", "agent"].map(String::from).to_vec(),
            shell_command: Some(["sh", "-c", "exec cat"].map(String::from).to_vec()),
            ..Default::default()
        };
        let store = ProjectStore {
            projects: vec![project("p", dir)],
        };
        let mut app = App::new(store, settings);
        app.sessions.insert(
            "p".into(),
            ProjectSession {
                tabs: vec![
                    agent_tab("main", "t-main", dir),
                    agent_tab("side", "t-side", dir),
                    md_tab(dir),
                ],
                shell: Some(cat_pane(dir)),
                ..Default::default()
            },
        );
        app.open_project("p", Some(0), BODY);
        app
    }

    fn api(app: &mut App, req: Request) -> ApiReply {
        app.handle_api(req, BODY)
    }

    #[test]
    fn sessions_lists_every_tab_and_the_shell_with_usable_ids() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = open_app(dir.path());
        let body = api(&mut app, Request::Sessions).unwrap();
        let sessions: Vec<SessionEntry> = serde_json::from_value(body["sessions"].clone()).unwrap();
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["t-main", "t-side", "md:p:2", "shell:p"]);

        let main = &sessions[0];
        assert_eq!(main.kind, "agent");
        assert_eq!(main.harness.as_deref(), Some("claude"));
        assert!(main.running && main.active && main.focused);
        assert_eq!(main.session_id.as_deref(), Some("sess-main"));
        assert!(main.status.is_some());
        assert!(!sessions[1].active, "only the shown tab is active");
        assert_eq!(sessions[2].kind, "markdown");
        assert!(sessions[2].path.as_deref().unwrap().ends_with("notes.md"));
        let shell = &sessions[3];
        assert_eq!((shell.kind.as_str(), shell.running), ("shell", true));
        assert_eq!(shell.project_path, dir.path().to_string_lossy());
        assert!(shell.status.is_none());
    }

    #[test]
    fn send_types_into_a_session_and_submit_presses_enter_after_a_pause() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = open_app(dir.path());
        api(
            &mut app,
            Request::Send(SendRequest {
                session: "shell:p".into(),
                text: "hello there".into(),
                submit: true,
                focus: false,
            }),
        )
        .unwrap();
        let shell = app.sessions["p"].shell.as_ref().unwrap();
        // Typed (echoed once) but not yet submitted: the Enter waits.
        let typed = wait_for(shell, |s| s.contains("hello there"));
        assert_eq!(typed.matches("hello there").count(), 1, "{typed}");
        assert_eq!(app.pending_submits.len(), 1);

        app.flush_pending_submits(Instant::now() + SUBMIT_DELAY);
        assert!(app.pending_submits.is_empty());
        let shell = app.sessions["p"].shell.as_ref().unwrap();
        // Submitted: `cat` prints the line back.
        let done = wait_for(shell, |s| s.matches("hello there").count() == 2);
        assert_eq!(done.matches("hello there").count(), 2, "{done}");
        assert_eq!(app.focus, Focus::Claude, "send without --focus stays put");
    }

    #[test]
    fn send_reports_what_it_cannot_do() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = open_app(dir.path());
        let send = |session: &str| {
            Request::Send(SendRequest {
                session: session.into(),
                text: "x".into(),
                submit: false,
                focus: false,
            })
        };
        let err = api(&mut app, send("nope")).unwrap_err();
        assert!(err.starts_with("no session 'nope'"), "{err}");
        let err = api(&mut app, send("md:p:2")).unwrap_err();
        assert_eq!(err, "a markdown tab can't take text");
        let err = api(&mut app, send("md:p:0")).unwrap_err();
        assert!(
            err.starts_with("no session"),
            "index 0 isn't markdown: {err}"
        );
        if let Some(Tab::Claude(t)) = app.sessions.get_mut("p").unwrap().tabs.get_mut(1) {
            t.pane = None;
        }
        let err = api(&mut app, send("t-side")).unwrap_err();
        assert_eq!(err, "that session isn't running");
    }

    #[test]
    fn focus_shows_the_session_and_hides_a_widget() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = open_app(dir.path());
        app.widgets.shown = Some(("w".into(), None));

        api(
            &mut app,
            Request::Focus(FocusRequest {
                session: "t-side".into(),
            }),
        )
        .unwrap();
        assert_eq!(app.sessions["p"].active_tab, 1);
        assert_eq!(app.focus, Focus::Claude);
        assert!(app.widgets.shown.is_none());

        api(
            &mut app,
            Request::Focus(FocusRequest {
                session: "shell:p".into(),
            }),
        )
        .unwrap();
        assert_eq!(app.focus, Focus::Shell);
    }

    #[test]
    fn a_notification_opens_its_target_and_comes_down() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = open_app(dir.path());
        let notify = |session: Option<&str>, project: Option<&str>| {
            Request::Banner(BannerRequest {
                title: "PR #12 is green".into(),
                body: None,
                session: session.map(String::from),
                project: project.map(String::from),
                widget: None,
                ttl: None,
            })
        };
        let id = api(&mut app, notify(Some("t-side"), None)).unwrap()["id"]
            .as_u64()
            .unwrap();
        assert_eq!(app.notifications.newest().unwrap().id, id);

        assert!(app.open_notification(BODY));
        assert_eq!(app.sessions["p"].active_tab, 1);
        assert!(app.notifications.is_empty());
        assert!(!app.open_notification(BODY), "nothing left to open");

        // A target that's gone is reported, and the banner still comes down.
        api(&mut app, notify(None, Some("gone"))).unwrap();
        assert!(app.open_notification(BODY));
        assert!(app.error.as_deref().unwrap().contains("no project 'gone'"));
        assert!(app.notifications.is_empty());

        // Dismiss by id; an unknown id is an error.
        let id = api(&mut app, notify(None, None)).unwrap()["id"]
            .as_u64()
            .unwrap();
        api(
            &mut app,
            Request::Dismiss(crate::ipc::DismissRequest { id }),
        )
        .unwrap();
        assert!(app.notifications.is_empty());
        assert!(
            api(
                &mut app,
                Request::Dismiss(crate::ipc::DismissRequest { id })
            )
            .is_err()
        );
    }

    #[test]
    fn tsv_rows_stay_one_line() {
        let s = SessionEntry {
            id: "t1".into(),
            kind: "agent".into(),
            harness: None,
            project: "p".into(),
            project_path: "/p".into(),
            name: "two\twords\nhere".into(),
            status: Some("busy".into()),
            running: true,
            active: false,
            focused: false,
            session_id: None,
            path: None,
        };
        assert_eq!(tsv_row(&s), "t1\tagent\tp\ttwo words here\tbusy");
    }
}
