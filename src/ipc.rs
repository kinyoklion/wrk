//! Per-instance IPC so `wrk view <file>` run inside a wrk pane can tell the
//! running TUI to open a markdown tab.
//!
//! Each running TUI binds a Unix domain socket at
//! `<runtime>/wrk/sock/wrk-<pid>.sock` and exports its path to every spawned
//! PTY as `WRK_SOCK` (with `WRK_PROJECT` naming the owning project). A `wrk
//! view` invoked inside such a pane connects, writes one line of JSON, and
//! disconnects; the server accepts one request per connection and forwards it
//! to the event loop over a channel.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

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

/// Directory holding per-instance sockets (sibling of the status dir).
pub fn socket_dir() -> PathBuf {
    crate::status::status_dir()
        .parent()
        .map(|p| p.join("sock"))
        .unwrap_or_else(|| PathBuf::from("/tmp/wrk/sock"))
}

/// Socket path for the given process id.
pub fn socket_path(pid: u32) -> PathBuf {
    socket_dir().join(format!("wrk-{pid}.sock"))
}

/// Bind this process's socket and spawn an accept thread that forwards each
/// parsed [`OpenRequest`] over the returned channel. Returns the socket path
/// (for the env export and cleanup on exit) and the receiver (drained by the
/// event loop).
pub fn serve() -> Result<(PathBuf, Receiver<OpenRequest>)> {
    let dir = socket_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = socket_path(std::process::id());
    // A stale socket from a crashed prior instance with the same pid would make
    // bind fail with EADDRINUSE; clear it first.
    let _ = std::fs::remove_file(&path);
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;

    let (tx, rx) = channel::<OpenRequest>();
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

fn read_request(stream: UnixStream) -> Option<OpenRequest> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    serde_json::from_str(line.trim()).ok()
}

/// Client side: connect to `sock` and send one open request. Fails fast when
/// the socket is gone (e.g. a stale `WRK_SOCK` from an instance that exited),
/// so the caller can fall back to the standalone viewer.
pub fn send_open(sock: &Path, req: &OpenRequest) -> Result<()> {
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
        let req = OpenRequest {
            path: "/tmp/doc.md".to_string(),
            project: Some("web".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: OpenRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn project_defaults_to_none_when_absent() {
        let req: OpenRequest = serde_json::from_str(r#"{"path":"/a/b.md"}"#).unwrap();
        assert_eq!(req.path, "/a/b.md");
        assert_eq!(req.project, None);
    }

    #[test]
    fn socket_path_includes_pid() {
        let p = socket_path(4242);
        assert!(p.to_string_lossy().ends_with("wrk-4242.sock"));
    }
}
