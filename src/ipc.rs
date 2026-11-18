//! Per-instance IPC so external `wrk` invocations can talk to the running TUI.
//!
//! Each running TUI binds a Unix domain socket at
//! `<runtime>/wrk/sock/wrk-<pid>.sock` and exports its path to every spawned
//! PTY as `WRK_SOCK` (with `WRK_PROJECT` naming the owning project and, for
//! Claude tabs, `WRK_TAB` identifying the tab). Callers connect, write one line
//! of JSON, and disconnect: `wrk view <file>` sends a [`Request::Open`] that
//! becomes a markdown tab, `wrk hook <kind>` (from Claude Code hooks) sends a
//! [`Request::Status`] that updates the originating tab's sidebar status, and
//! `wrk notify` sends a [`Request::Notify`] for the status bar. A
//! [`Request::Confirm`] (`wrk confirm`) gets an answer: wrk asks the user, then
//! writes one [`ConfirmReply`] line back on the same connection. The server
//! accepts one request per connection and forwards it, with the connection, to
//! the event loop over a channel.
//!
//! The **API** requests (`wrk api …`, used by widgets and scripts — see
//! [`Request::is_api`]) are answered too: the event loop handles them and
//! writes one line of JSON back, `{"ok":true,…}` or `{"ok":false,"error":"…"}`
//! ([`Reply::respond`]), before closing.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::status::StatusKind;

/// One line-JSON message from an external `wrk` invocation to the running TUI.
/// Internally tagged by `cmd` so a single socket carries both request kinds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    /// Open a file as a markdown tab (`wrk view`).
    Open(OpenRequest),
    /// Update a Claude tab's status (`wrk hook`).
    Status(StatusUpdate),
    /// Start or end an in-TUI code review (`wrk review`).
    Review(ReviewRequest),
    /// Ask the user a yes/no question (`wrk confirm`); answered with a
    /// [`ConfirmReply`] line on the same connection.
    Confirm(ConfirmRequest),
    /// Show a message in the status bar (`wrk notify`).
    Notify(NotifyRequest),
    /// API: list the running sessions. Replies `{"sessions":[…]}`.
    Sessions,
    /// API: type text into a session.
    Send(SendRequest),
    /// API: show a session (open its project and tab, focus it).
    Focus(FocusRequest),
    /// API: post a notification banner. Replies `{"id":N}`.
    Banner(BannerRequest),
    /// API: dismiss a notification banner by id.
    Dismiss(DismissRequest),
    /// API: summon a widget.
    Show(ShowRequest),
    /// API: hide the widget on screen.
    Hide,
    /// API: have a session plugin create something, as its creation form
    /// does. Answered once the plugin has: `{"project","session"?,"message"?}`.
    Create(CreateRequest),
}

impl Request {
    /// Whether this is an API request (`wrk api …`), answered with an
    /// `{"ok":…}` line through [`Reply::respond`]. The others are
    /// fire-and-forget, except a [`Request::Confirm`], which has its own
    /// answer ([`Reply::answer`]).
    pub fn is_api(&self) -> bool {
        matches!(
            self,
            Request::Sessions
                | Request::Send(_)
                | Request::Focus(_)
                | Request::Banner(_)
                | Request::Dismiss(_)
                | Request::Show(_)
                | Request::Hide
                | Request::Create(_)
        )
    }
}

/// A yes/no question for the user, from a process in one of wrk's panes (a
/// pane's command asking before it does something on the user's behalf). wrk
/// shows it as a prompt, queued behind any prompt already open, and answers
/// once the user picks. A requester that hangs up first withdraws it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmRequest {
    /// Project the request came from (from `WRK_PROJECT`), named on the prompt.
    #[serde(default)]
    pub project: Option<String>,
    /// The question, one line.
    pub title: String,
    /// Detail shown under the question, a line each.
    #[serde(default)]
    pub lines: Vec<String>,
    /// What `y` does, e.g. `download` (default `yes`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accept: Option<String>,
    /// What `n` / Esc does, e.g. `decline` (default `no`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decline: Option<String>,
}

/// wrk's answer to a [`ConfirmRequest`]. Nothing comes back (the connection
/// just closes) when wrk exits without the user answering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmReply {
    pub accepted: bool,
}

/// A message for the status bar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyRequest {
    /// Project the message is about (from `WRK_PROJECT`).
    #[serde(default)]
    pub project: Option<String>,
    pub message: String,
    /// Show it as an error rather than as information.
    #[serde(default)]
    pub error: bool,
}

/// One request off the socket, with the connection it came on (kept open for
/// the answer to a [`Request::Confirm`] or an API request; dropping it closes
/// the connection).
pub struct Incoming {
    pub request: Request,
    pub reply: Reply,
}

impl Incoming {
    /// Answer an API request (see [`Reply::respond`]).
    pub fn respond(mut self, reply: ApiReply) {
        self.reply.respond(reply);
    }
}

/// The answer to an API request: a JSON object (sent with `"ok": true`), or
/// an error message.
pub type ApiReply = Result<serde_json::Value, String>;

/// How long the event loop may block writing an answer, so a requester that
/// stops reading can't wedge the UI thread.
const WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// How long `call` waits for the reply line.
const CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// A `create` is answered once the plugin has made the thing, which can take
/// as long as the plugin's own request timeout.
const CREATE_CALL_TIMEOUT: Duration = Duration::from_secs(180);

/// The connection a request came on, for writing the one answer back.
#[derive(Debug, Default)]
pub struct Reply {
    conn: Option<UnixStream>,
}

impl Reply {
    pub fn new(conn: UnixStream) -> Self {
        Reply { conn: Some(conn) }
    }

    /// Answer a confirm and close the connection. Best effort: a requester
    /// that already left just misses it.
    pub fn answer(&mut self, accepted: bool) {
        let line = serde_json::to_string(&ConfirmReply { accepted }).unwrap_or_default();
        self.write_line(&line);
    }

    /// Answer an API request with `{"ok":true,…body}` or
    /// `{"ok":false,"error":…}` and close the connection.
    pub fn respond(&mut self, reply: ApiReply) {
        let line = match reply {
            Ok(body) => ok_line(body),
            Err(message) => error_line(message),
        };
        self.write_line(&line);
    }

    /// Write the one answer line and close. Best effort: a requester that
    /// already left just misses it.
    fn write_line(&mut self, line: &str) {
        if let Some(mut conn) = self.conn.take() {
            let _ = conn.set_write_timeout(Some(WRITE_TIMEOUT));
            let _ = writeln!(conn, "{line}");
        }
    }

    /// Has the requester hung up (so a prompt waiting on it is moot)? A
    /// requester sends nothing after its request, so a non-blocking read sees
    /// either nothing yet (still there) or end of file (gone).
    pub fn withdrawn(&self) -> bool {
        let Some(conn) = &self.conn else {
            return false;
        };
        if conn.set_nonblocking(true).is_err() {
            return true;
        }
        let mut buf = [0u8; 64];
        let gone = match (&*conn).read(&mut buf) {
            Ok(0) => true,
            Ok(_) => false,
            Err(e) => e.kind() != std::io::ErrorKind::WouldBlock,
        };
        let _ = conn.set_nonblocking(false);
        gone
    }
}

/// Type `text` into a session's terminal, as if pasted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendRequest {
    /// Session id, as listed by `sessions`.
    pub session: String,
    pub text: String,
    /// Press Enter after the text (sent a moment later, so the program has
    /// taken the paste first).
    #[serde(default)]
    pub submit: bool,
    /// Also show the session (and hide the widget on screen).
    #[serde(default)]
    pub focus: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusRequest {
    pub session: String,
}

/// A notification banner (`wrk api banner`). Activating it opens its target:
/// the session if set, else the project, else the widget; with none it only
/// dismisses. Not to be confused with [`NotifyRequest`], a one-line status-bar
/// message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BannerRequest {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widget: Option<String>,
    /// Seconds until it disappears on its own; unset → until dismissed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DismissRequest {
    pub id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShowRequest {
    pub widget: String,
}

/// Ask a session plugin to create a project or session, with the values its
/// creation form would have sent (`wrk api create`). What it makes opens in
/// wrk once the plugin lists it, unless `open` is false.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateRequest {
    /// The plugin, as configured (`[plugin.<name>]`).
    pub plugin: String,
    /// One of the plugin's listed projects (its wrk key, `<plugin>/<id>`), for
    /// a create made from inside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Form field id → value.
    #[serde(default)]
    pub values: std::collections::BTreeMap<String, String>,
    #[serde(default = "default_true")]
    pub open: bool,
}

fn default_true() -> bool {
    true
}

/// A request to open a file in a running wrk instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenRequest {
    /// Absolute path to the file to open.
    pub path: String,
    /// Project the request came from (from `WRK_PROJECT`); the tab opens in this
    /// project's session. `None` falls back to the active project.
    #[serde(default)]
    pub project: Option<String>,
}

/// A status push from an agent hook (`wrk hook`), tagging the originating tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusUpdate {
    /// The tab's opaque id (from `WRK_TAB`).
    pub tab: String,
    /// The state transition.
    pub kind: StatusKind,
    /// The agent's own session id, read from the hook event JSON on stdin. Lets
    /// wrk follow a tab whose session changed (Claude `/clear`, `/resume`) and
    /// learn a Kimi tab's id, so a restart resumes the tab's current session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The hook event's `source` (Claude `SessionStart`: `startup`, `resume`,
    /// `clear`, `compact`), used to tell a continued conversation from a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// A `wrk review` request from a Claude session: start a review of its project's
/// diff, or end the active one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRequest {
    /// Originating Claude tab (`WRK_TAB`), so comments can be reported back to it.
    #[serde(default)]
    pub tab: Option<String>,
    /// Project (`WRK_PROJECT`) whose working tree is reviewed; `None` → active.
    #[serde(default)]
    pub project: Option<String>,
    /// Directory `wrk review start` was invoked in. The diff is built against
    /// the git repo enclosing it (which may be a worktree nested inside a
    /// non-git project directory); `None` → fall back to the project path.
    #[serde(default)]
    pub cwd: Option<String>,
    pub kind: ReviewKind,
}

/// Start (with an optional git target) or end a review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum ReviewKind {
    /// Open the review overlay. `target` is the raw `wrk review start [target]`
    /// argument (empty → uncommitted vs HEAD).
    Start {
        #[serde(default)]
        target: Option<String>,
    },
    /// Close the active review overlay.
    End,
}

/// Directory holding per-instance sockets.
pub fn socket_dir() -> PathBuf {
    crate::status::runtime_dir().join("sock")
}

/// Socket path for the given process id.
pub fn socket_path(pid: u32) -> PathBuf {
    socket_dir().join(format!("wrk-{pid}.sock"))
}

/// Parse the owning pid out of a socket path (`…/wrk-<pid>.sock`).
pub fn pid_from_socket(path: &str) -> Option<u32> {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("wrk-"))
        .and_then(|n| n.strip_suffix(".sock"))
        .and_then(|n| n.parse().ok())
}

/// Is a live wrk actually listening on this socket path? A bound listener
/// accepts the connection (even mid-request, via the listen backlog); a leftover
/// socket file whose owner is gone refuses it (`ECONNREFUSED`). This is the
/// authoritative liveness test — unlike a pid check it can't be fooled by pid
/// reuse (an unrelated process inheriting a dead wrk's pid). The probe connects
/// and immediately drops; the peer's accept loop reads an empty line and ignores
/// it, so a live instance is never disturbed.
fn socket_is_live(path: &Path) -> bool {
    UnixStream::connect(path).is_ok()
}

/// Remove socket files left behind by wrk instances that are no longer running
/// (unclean shutdown leaks `wrk-<pid>.sock`; nothing else reaps them). Liveness
/// is probed by connecting, not by checking the owning pid — a crashed wrk whose
/// pid was later reused by an unrelated process would pass a `/proc` check and
/// leave the dead socket lingering (then `wrk view`/`wrk hook` connect to a path
/// with no wrk behind it). Best-effort; called once at startup.
pub fn sweep_stale_sockets() {
    let Ok(entries) = std::fs::read_dir(socket_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.to_str().and_then(pid_from_socket).is_some() && !socket_is_live(&path) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Bind this process's socket and spawn an accept thread that forwards each
/// parsed [`Request`] (with its connection) over the returned channel. Returns
/// the socket path (for the env export and cleanup on exit) and the receiver
/// (drained by the event loop).
pub fn serve() -> Result<(PathBuf, Receiver<Incoming>)> {
    let dir = socket_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    serve_at(socket_path(std::process::id()))
}

/// [`serve`] on an explicit path (tests bind under a temp dir).
fn serve_at(path: PathBuf) -> Result<(PathBuf, Receiver<Incoming>)> {
    // A stale socket from a crashed prior instance with the same pid would make
    // bind fail with EADDRINUSE; clear it first.
    let _ = std::fs::remove_file(&path);
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;

    let (tx, rx) = channel::<Incoming>();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                continue;
            };
            // One slow or silent client must not hold up the rest.
            let tx = tx.clone();
            std::thread::spawn(move || {
                if let Some(request) = read_request(&stream) {
                    let _ = tx.send(Incoming {
                        request,
                        reply: Reply::new(stream),
                    });
                }
            });
        }
    });
    Ok((path, rx))
}

/// The connection's one request line. A client that never finishes it is
/// given up on after a while, so its thread doesn't linger. A malformed line
/// is answered with an error (an API caller is waiting to hear why) and
/// dropped; an empty one — a liveness probe — is dropped silently.
fn read_request(stream: &UnixStream) -> Option<Request> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let _ = stream.set_read_timeout(None);
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    match serde_json::from_str(line) {
        Ok(request) => Some(request),
        Err(e) => {
            let mut stream = stream;
            let _ = writeln!(stream, "{}", error_line(format!("malformed request: {e}")));
            None
        }
    }
}

fn ok_line(body: serde_json::Value) -> String {
    let mut reply = serde_json::Map::new();
    reply.insert("ok".into(), true.into());
    if let serde_json::Value::Object(fields) = body {
        reply.extend(fields);
    }
    serde_json::Value::Object(reply).to_string()
}

fn error_line(message: impl std::fmt::Display) -> String {
    serde_json::json!({ "ok": false, "error": message.to_string() }).to_string()
}

/// Client side of an API request: send it, wait for the reply line, and
/// return its body (the `ok` envelope removed) or the error wrk reported.
pub fn call(sock: &Path, req: &Request) -> Result<serde_json::Value> {
    let mut stream =
        UnixStream::connect(sock).with_context(|| format!("connecting to {}", sock.display()))?;
    let timeout = match req {
        Request::Create(_) => CREATE_CALL_TIMEOUT,
        _ => CALL_TIMEOUT,
    };
    stream.set_read_timeout(Some(timeout))?;
    let mut payload = serde_json::to_string(req)?;
    payload.push('\n');
    stream.write_all(payload.as_bytes())?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .context("reading wrk's reply")?;
    parse_reply(&line)
}

/// Unwrap a reply line: the body of an `ok` reply, or its error as `Err`.
fn parse_reply(line: &str) -> Result<serde_json::Value> {
    let line = line.trim();
    if line.is_empty() {
        anyhow::bail!("wrk closed the connection without replying");
    }
    let mut value: serde_json::Value =
        serde_json::from_str(line).with_context(|| format!("malformed reply: {line}"))?;
    let Some(fields) = value.as_object_mut() else {
        anyhow::bail!("malformed reply: {line}");
    };
    if fields.remove("ok").and_then(|ok| ok.as_bool()) != Some(true) {
        let message = fields
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("wrk reported an unspecified error");
        anyhow::bail!("{message}");
    }
    Ok(value)
}

/// Client side: connect to `sock` and send one request. Fails fast when the
/// socket is gone (e.g. a stale `WRK_SOCK` from an instance that exited), so the
/// caller can fall back (e.g. to the standalone viewer) or silently ignore it.
pub fn send(sock: &Path, req: &Request) -> Result<()> {
    let mut stream =
        UnixStream::connect(sock).with_context(|| format!("connecting to {}", sock.display()))?;
    let mut payload = serde_json::to_string(req)?;
    payload.push('\n');
    stream.write_all(payload.as_bytes())?;
    stream.flush()?;
    Ok(())
}

/// Client side of a [`Request::Confirm`]: send it and wait for the answer.
/// `Ok(None)` when wrk closed the connection without one (it exited, or the
/// request didn't parse).
pub fn ask(sock: &Path, req: &ConfirmRequest) -> Result<Option<bool>> {
    let mut stream =
        UnixStream::connect(sock).with_context(|| format!("connecting to {}", sock.display()))?;
    let mut payload = serde_json::to_string(&Request::Confirm(req.clone()))?;
    payload.push('\n');
    stream.write_all(payload.as_bytes())?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    if line.trim().is_empty() {
        return Ok(None);
    }
    let reply: ConfirmReply = serde_json::from_str(line.trim()).context("wrk's answer")?;
    Ok(Some(reply.accepted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_request_json_round_trip() {
        let req = Request::Open(OpenRequest {
            path: "/tmp/doc.md".to_string(),
            project: Some("web".to_string()),
        });
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""cmd":"open""#));
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn status_request_json_round_trip() {
        let req = Request::Status(StatusUpdate {
            tab: "tab3".to_string(),
            kind: StatusKind::Waiting,
            session_id: None,
            source: None,
        });
        let json = serde_json::to_string(&req).unwrap();
        // No session id → the field is omitted, so the Claude push is unchanged.
        assert_eq!(json, r#"{"cmd":"status","tab":"tab3","kind":"waiting"}"#);
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn status_request_carries_session_id_when_present() {
        let req = Request::Status(StatusUpdate {
            tab: "tab0".to_string(),
            kind: StatusKind::Stopped,
            session_id: Some("session_abc".to_string()),
            source: None,
        });
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""session_id":"session_abc""#));
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), req);
        // A push without the field still parses (default None) — Claude hooks.
        let back: Request =
            serde_json::from_str(r#"{"cmd":"status","tab":"t","kind":"busy"}"#).unwrap();
        assert_eq!(
            back,
            Request::Status(StatusUpdate {
                tab: "t".to_string(),
                kind: StatusKind::Busy,
                session_id: None,
                source: None,
            })
        );
    }

    #[test]
    fn review_request_json_round_trip() {
        let start = Request::Review(ReviewRequest {
            tab: Some("tab7".into()),
            project: Some("web".into()),
            cwd: Some("/home/me/proj/worktree".into()),
            kind: ReviewKind::Start {
                target: Some("main..HEAD".into()),
            },
        });
        let json = serde_json::to_string(&start).unwrap();
        assert!(json.contains(r#""cmd":"review""#));
        assert!(json.contains(r#""action":"start""#));
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), start);

        // End needs no fields beyond the action tag; absent tab/project/cwd default.
        let end: Request =
            serde_json::from_str(r#"{"cmd":"review","kind":{"action":"end"}}"#).unwrap();
        assert_eq!(
            end,
            Request::Review(ReviewRequest {
                tab: None,
                project: None,
                cwd: None,
                kind: ReviewKind::End,
            })
        );
    }

    #[test]
    fn open_project_defaults_to_none_when_absent() {
        let req: Request = serde_json::from_str(r#"{"cmd":"open","path":"/a/b.md"}"#).unwrap();
        assert_eq!(
            req,
            Request::Open(OpenRequest {
                path: "/a/b.md".to_string(),
                project: None,
            })
        );
    }

    #[test]
    fn confirm_and_notify_round_trip_with_defaults() {
        let req: Request = serde_json::from_str(r#"{"cmd":"confirm","title":"send?"}"#).unwrap();
        assert_eq!(
            req,
            Request::Confirm(ConfirmRequest {
                project: None,
                title: "send?".into(),
                lines: vec![],
                accept: None,
                decline: None,
            })
        );
        let note: Request =
            serde_json::from_str(r#"{"cmd":"notify","message":"saved","project":"p"}"#).unwrap();
        assert_eq!(
            note,
            Request::Notify(NotifyRequest {
                project: Some("p".into()),
                message: "saved".into(),
                error: false,
            })
        );
    }

    #[test]
    fn a_confirm_is_answered_on_its_own_connection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wrk-1.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let Some(Request::Confirm(req)) = read_request(&stream) else {
                panic!("not a confirm");
            };
            assert_eq!(req.lines, vec!["a.md".to_string()]);
            let mut reply = Reply::new(stream);
            assert!(!reply.withdrawn(), "the asker is still waiting");
            reply.answer(true);
        });
        let req = ConfirmRequest {
            project: Some("web".into()),
            title: "download?".into(),
            lines: vec!["a.md".into()],
            accept: Some("download".into()),
            decline: None,
        };
        assert_eq!(ask(&path, &req).unwrap(), Some(true));
        server.join().unwrap();
    }

    #[test]
    fn a_requester_that_hangs_up_has_withdrawn() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let mut reply = Reply::new(ours);
        assert!(!reply.withdrawn());
        drop(theirs);
        // A concurrent test forking a child briefly holds copies of every fd
        // (until its exec), which can delay the EOF: poll.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !reply.withdrawn() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(reply.withdrawn());
        // Answering a requester that left is harmless.
        reply.answer(false);
        assert!(!reply.withdrawn(), "answered: nothing left to withdraw");
    }

    #[test]
    fn socket_path_includes_pid() {
        let p = socket_path(4242);
        assert!(p.to_string_lossy().ends_with("wrk-4242.sock"));
    }

    #[test]
    fn socket_is_live_only_when_a_listener_is_bound() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wrk-99.sock");

        // No file yet → not live.
        assert!(!socket_is_live(&path));

        // A bound listener → live.
        let listener = UnixListener::bind(&path).unwrap();
        assert!(socket_is_live(&path));

        // Dropping the listener leaves the socket *file* behind (as an unclean
        // shutdown does) but nothing listens → connect refused → not live.
        drop(listener);
        assert!(path.exists());
        // A child forked by a concurrently running test (e.g. `git`) holds
        // copies of every open fd until its exec, which can briefly keep the
        // dropped listener connectable; allow that moment to pass.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while socket_is_live(&path) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!socket_is_live(&path));
    }

    #[test]
    fn api_requests_use_flat_cmd_tagged_json() {
        let send: Request =
            serde_json::from_str(r#"{"cmd":"send","session":"tab3","text":"hi","submit":true}"#)
                .unwrap();
        assert_eq!(
            send,
            Request::Send(SendRequest {
                session: "tab3".into(),
                text: "hi".into(),
                submit: true,
                focus: false,
            })
        );
        assert!(send.is_api());
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"sessions"}"#).unwrap(),
            Request::Sessions
        );
        assert_eq!(
            serde_json::to_string(&Request::Hide).unwrap(),
            r#"{"cmd":"hide"}"#
        );
        let banner: Request = serde_json::from_str(r#"{"cmd":"banner","title":"CI"}"#).unwrap();
        assert!(matches!(banner, Request::Banner(b) if b.title == "CI" && b.ttl.is_none()));
        assert!(parses_as_api(r#"{"cmd":"banner","title":"CI"}"#));
        // `notify` is the status-bar message (`wrk notify`), not a banner: a
        // banner-shaped `notify` is rejected rather than misread.
        assert!(serde_json::from_str::<Request>(r#"{"cmd":"notify","title":"CI"}"#).is_err());
        // The fire-and-forget requests, and a confirm, aren't API requests.
        assert!(
            !Request::Status(StatusUpdate {
                tab: "t".into(),
                kind: StatusKind::Busy,
                session_id: None,
                source: None,
            })
            .is_api()
        );
        assert!(!parses_as_api(r#"{"cmd":"notify","message":"saved"}"#));
        assert!(!parses_as_api(r#"{"cmd":"confirm","title":"send?"}"#));
    }

    #[test]
    fn a_create_names_its_plugin_and_opens_by_default() {
        let create: Request = serde_json::from_str(
            r#"{"cmd":"create","plugin":"boxes","values":{"box":"b1","name":"review"}}"#,
        )
        .unwrap();
        assert!(create.is_api());
        let Request::Create(req) = create else {
            panic!("{create:?}");
        };
        assert_eq!(req.plugin, "boxes");
        assert_eq!(req.project, None);
        assert_eq!(req.values["name"], "review");
        assert!(req.open);
        let quiet: Request =
            serde_json::from_str(r#"{"cmd":"create","plugin":"boxes","open":false}"#).unwrap();
        assert!(matches!(quiet, Request::Create(r) if !r.open && r.values.is_empty()));
    }

    fn parses_as_api(json: &str) -> bool {
        serde_json::from_str::<Request>(json).unwrap().is_api()
    }

    /// An API call goes through the socket to the event loop (here, a thread
    /// standing in for it) and its reply comes back, ok or error; a
    /// fire-and-forget request still arrives, and is never answered.
    #[test]
    fn api_calls_get_replies_through_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let (path, rx) = serve_at(dir.path().join("t.sock")).unwrap();
        let fake_loop = std::thread::spawn(move || {
            let (mut fire_and_forget, mut hidden) = (0, false);
            // Each connection is read on its own thread, so the Open and the
            // Hide may arrive in either order: run until both have.
            while !(hidden && fire_and_forget == 1) {
                let incoming = rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .expect("a request");
                let reply = match &incoming.request {
                    Request::Sessions => Ok(serde_json::json!({ "sessions": ["a"] })),
                    Request::Focus(f) => Err(format!("no session '{}'", f.session)),
                    Request::Hide => {
                        hidden = true;
                        Ok(serde_json::json!({}))
                    }
                    _ => {
                        fire_and_forget += 1;
                        continue;
                    }
                };
                incoming.respond(reply);
            }
            fire_and_forget
        });

        let body = call(&path, &Request::Sessions).unwrap();
        assert_eq!(body, serde_json::json!({ "sessions": ["a"] }));
        let err = call(
            &path,
            &Request::Focus(FocusRequest {
                session: "nope".into(),
            }),
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "no session 'nope'");
        send(
            &path,
            &Request::Open(OpenRequest {
                path: "/x.md".into(),
                project: None,
            }),
        )
        .unwrap();
        call(&path, &Request::Hide).unwrap();
        assert_eq!(fake_loop.join().unwrap(), 1);
    }

    #[test]
    fn a_malformed_api_request_is_told_why() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _rx) = serve_at(dir.path().join("t.sock")).unwrap();
        let mut stream = UnixStream::connect(&path).unwrap();
        stream.write_all(b"{\"cmd\":\"bogus\"}\n").unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        let err = parse_reply(&line).unwrap_err().to_string();
        assert!(err.starts_with("malformed request"), "{err}");
    }

    #[test]
    fn pid_round_trips_through_socket_path() {
        let p = socket_path(4242);
        assert_eq!(pid_from_socket(&p.to_string_lossy()), Some(4242));
        assert_eq!(
            pid_from_socket("/run/user/1000/wrk/sock/wrk-77.sock"),
            Some(77)
        );
        assert_eq!(pid_from_socket("/tmp/not-a-wrk-socket"), None);
        assert_eq!(pid_from_socket("wrk-abc.sock"), None);
    }
}
