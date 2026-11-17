//! Per-instance IPC so external `wrk` invocations can talk to the running TUI.
//!
//! Each running TUI binds a Unix domain socket at
//! `<runtime>/wrk/sock/wrk-<pid>.sock` and exports its path to every spawned
//! PTY as `WRK_SOCK` (with `WRK_PROJECT` naming the owning project and, for
//! Claude tabs, `WRK_TAB` identifying the tab). Two callers connect, write one
//! line of JSON, and disconnect: `wrk view <file>` sends a [`Request::Open`]
//! that becomes a markdown tab, and `wrk hook <kind>` (from Claude Code hooks)
//! sends a [`Request::Status`] that updates the originating tab's sidebar
//! status. The server accepts one request per connection and forwards it to the
//! event loop over a channel.
//!
//! The **API** requests (`wrk api …`, used by widgets and scripts — see
//! [`Request::expects_reply`]) are answered: the event loop handles them and
//! the server writes one line of JSON back, `{"ok":true,…}` or
//! `{"ok":false,"error":"…"}`, before closing.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
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
    /// API: list the running sessions. Replies `{"sessions":[…]}`.
    Sessions,
    /// API: type text into a session.
    Send(SendRequest),
    /// API: show a session (open its project and tab, focus it).
    Focus(FocusRequest),
    /// API: post a notification banner. Replies `{"id":N}`.
    Notify(NotifyRequest),
    /// API: dismiss a notification by id.
    Dismiss(DismissRequest),
    /// API: summon a widget.
    Show(ShowRequest),
    /// API: hide the widget on screen.
    Hide,
}

impl Request {
    /// Whether the sender waits for a reply line (the API requests). The
    /// others are fire-and-forget: their senders disconnect after writing.
    pub fn expects_reply(&self) -> bool {
        !matches!(
            self,
            Request::Open(_) | Request::Status(_) | Request::Review(_)
        )
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

/// A notification banner. Activating it opens its target: the session if set,
/// else the project, else the widget; with none it only dismisses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyRequest {
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

/// The answer to an API request: a JSON object (sent with `"ok": true`), or
/// an error message.
pub type Reply = Result<serde_json::Value, String>;

/// A request drained by the event loop. API requests carry where to send
/// their [`Reply`]; the connection waits on it.
pub struct Incoming {
    pub request: Request,
    pub reply: Option<Sender<Reply>>,
}

impl Incoming {
    /// Answer an API request (a no-op for fire-and-forget ones).
    pub fn respond(self, reply: Reply) {
        if let Some(tx) = self.reply {
            let _ = tx.send(reply);
        }
    }
}

/// How long a connection waits for the event loop to answer an API request.
/// The loop drains requests every tick (~33 ms), so this only trips if the UI
/// thread is wedged.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long `call` waits for the reply line.
const CALL_TIMEOUT: Duration = Duration::from_secs(15);
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
/// parsed [`Request`] over the returned channel. Returns the socket path (for
/// the env export and cleanup on exit) and the receiver (drained by the event
/// loop).
///
/// Fire-and-forget requests are forwarded in arrival order on the accept
/// thread, so two quick `wrk hook` pushes can't be reordered. An API request
/// gets its own thread, which waits for the event loop's [`Reply`] and writes
/// it back, so a slow answer never holds up other connections.
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
            let Some((request, stream)) = read_request(stream) else {
                continue;
            };
            if !request.expects_reply() {
                let incoming = Incoming {
                    request,
                    reply: None,
                };
                if tx.send(incoming).is_err() {
                    break; // receiver gone → the app is shutting down
                }
                continue;
            }
            let tx = tx.clone();
            std::thread::spawn(move || answer(request, stream, &tx));
        }
    });
    Ok((path, rx))
}

/// Read one request line. A malformed one is answered with an error (an API
/// caller is waiting to hear why) and dropped; an empty one — a liveness
/// probe — is dropped silently.
fn read_request(stream: UnixStream) -> Option<(Request, UnixStream)> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut stream = reader.into_inner();
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    match serde_json::from_str(line) {
        Ok(request) => Some((request, stream)),
        Err(e) => {
            let _ = writeln!(stream, "{}", error_line(format!("malformed request: {e}")));
            None
        }
    }
}

/// Hand an API request to the event loop and write its reply back.
fn answer(request: Request, mut stream: UnixStream, tx: &Sender<Incoming>) {
    let (reply_tx, reply_rx) = channel();
    let incoming = Incoming {
        request,
        reply: Some(reply_tx),
    };
    let line = if tx.send(incoming).is_err() {
        error_line("wrk is shutting down")
    } else {
        match reply_rx.recv_timeout(REPLY_TIMEOUT) {
            Ok(Ok(body)) => ok_line(body),
            Ok(Err(message)) => error_line(message),
            Err(_) => error_line("wrk didn't answer in time"),
        }
    };
    let _ = writeln!(stream, "{line}");
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
    stream.set_read_timeout(Some(CALL_TIMEOUT))?;
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
        assert!(send.expects_reply());
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"sessions"}"#).unwrap(),
            Request::Sessions
        );
        assert_eq!(
            serde_json::to_string(&Request::Hide).unwrap(),
            r#"{"cmd":"hide"}"#
        );
        let notify: Request = serde_json::from_str(r#"{"cmd":"notify","title":"CI"}"#).unwrap();
        assert!(matches!(notify, Request::Notify(n) if n.title == "CI" && n.ttl.is_none()));
        // The fire-and-forget requests keep their old no-reply behavior.
        assert!(
            !Request::Status(StatusUpdate {
                tab: "t".into(),
                kind: StatusKind::Busy,
                session_id: None,
                source: None,
            })
            .expects_reply()
        );
    }

    /// An API call goes through the socket to the event loop (here, a thread
    /// standing in for it) and its reply comes back, ok or error; a
    /// fire-and-forget request still arrives with no reply attached.
    #[test]
    fn api_calls_get_replies_through_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let (path, rx) = serve_at(dir.path().join("t.sock")).unwrap();
        let fake_loop = std::thread::spawn(move || {
            let mut fire_and_forget = 0;
            while let Ok(incoming) = rx.recv() {
                let reply = match &incoming.request {
                    Request::Sessions => Ok(serde_json::json!({ "sessions": ["a"] })),
                    Request::Focus(f) => Err(format!("no session '{}'", f.session)),
                    Request::Hide => {
                        incoming.respond(Ok(serde_json::json!({})));
                        break;
                    }
                    _ => {
                        assert!(incoming.reply.is_none());
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
        // Hide last: the fake loop stops after answering it, and the Open sent
        // before it has been forwarded by then (one accept thread, in order).
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
