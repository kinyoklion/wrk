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

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};

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

/// A status push from a Claude Code hook, tagging the originating tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusUpdate {
    /// The tab's opaque id (from `WRK_TAB`).
    pub tab: String,
    /// The state transition.
    pub kind: StatusKind,
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

/// Remove socket files left behind by wrk instances that are no longer running
/// (unclean shutdown leaks `wrk-<pid>.sock`; nothing else reaps them). Best-
/// effort; called once at startup.
pub fn sweep_stale_sockets() {
    let Ok(entries) = std::fs::read_dir(socket_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(pid) = path.to_str().and_then(pid_from_socket)
            && !crate::status::pid_is_alive(pid)
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Bind this process's socket and spawn an accept thread that forwards each
/// parsed [`Request`] over the returned channel. Returns the socket path (for
/// the env export and cleanup on exit) and the receiver (drained by the event
/// loop).
pub fn serve() -> Result<(PathBuf, Receiver<Request>)> {
    let dir = socket_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = socket_path(std::process::id());
    // A stale socket from a crashed prior instance with the same pid would make
    // bind fail with EADDRINUSE; clear it first.
    let _ = std::fs::remove_file(&path);
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;

    let (tx, rx) = channel::<Request>();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                continue;
            };
            if let Some(req) = read_request(stream)
                && tx.send(req).is_err()
            {
                break; // receiver gone → the app is shutting down
            }
        }
    });
    Ok((path, rx))
}

fn read_request(stream: UnixStream) -> Option<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    serde_json::from_str(line.trim()).ok()
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
        });
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"cmd":"status","tab":"tab3","kind":"waiting"}"#);
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn review_request_json_round_trip() {
        let start = Request::Review(ReviewRequest {
            tab: Some("tab7".into()),
            project: Some("web".into()),
            kind: ReviewKind::Start {
                target: Some("main..HEAD".into()),
            },
        });
        let json = serde_json::to_string(&start).unwrap();
        assert!(json.contains(r#""cmd":"review""#));
        assert!(json.contains(r#""action":"start""#));
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), start);

        // End needs no fields beyond the action tag; absent tab/project default.
        let end: Request =
            serde_json::from_str(r#"{"cmd":"review","kind":{"action":"end"}}"#).unwrap();
        assert_eq!(
            end,
            Request::Review(ReviewRequest {
                tab: None,
                project: None,
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
