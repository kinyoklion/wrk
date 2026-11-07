//! End to end: launch the real plugin binary exactly the way wrk does
//! (`wrk_plugin::client::launch`) and drive it over its socket — the check
//! that was missing when the first, interpreter-based test plugin silently
//! failed to start on the host.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::Child;
use std::sync::mpsc;
use std::time::Duration;

use wrk_plugin::client::{call, launch};
use wrk_plugin::proto::{
    AttachKind, AttachRequest, AttachResponse, HelloResponse, ListResponse, ProjectPayload, Request,
};

/// Stops a launched daemon when dropped, pass or fail.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const TIMEOUT: Duration = Duration::from_secs(10);

fn start(dir: &Path, relay_status: bool) -> Daemon {
    let mut command = vec![env!("CARGO_BIN_EXE_wrk-claude-plugin").to_string()];
    if relay_status {
        command.push("--relay-status".into());
    }
    command.extend(["steam-run".into(), "claude".into()]);
    let socket = dir.join("claude-ext.sock");
    Daemon(launch(&command, &socket, &dir.join("claude-ext.log"), TIMEOUT).expect("it starts"))
}

fn project(dir: &Path) -> ProjectPayload {
    ProjectPayload {
        name: "svc".into(),
        path: dir.display().to_string(),
        config: Default::default(),
    }
}

/// An agent attach as wrk sends it — with `wrk_sock` only for a plugin that
/// advertised the `status` capability.
fn agent(dir: &Path, session_id: Option<&str>, wrk_sock: Option<&Path>) -> Request {
    Request::Attach(AttachRequest {
        project: project(dir),
        session_id: session_id.map(str::to_owned),
        kind: AttachKind::Agent,
        fresh: session_id.is_none(),
        name: "main".into(),
        wrk_sock: wrk_sock.map(|p| p.display().to_string()),
        wrk_tab: Some("tab3".into()),
    })
}

#[test]
fn launched_like_wrk_it_externalizes_claude_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("claude-ext.sock");
    let _daemon = start(dir.path(), false);

    let hello: HelloResponse = call(&socket, &Request::Hello, TIMEOUT).unwrap();
    assert_eq!(hello.name, "claude-ext");
    let caps = hello.capabilities;
    assert!(caps.list && caps.shell && caps.close, "{caps:?}");
    assert!(
        !caps.status,
        "agents report to wrk directly; the plugin pushes nothing"
    );

    // New session: wrk's own Claude flags and a minted id; the agent keeps the
    // WRK_* environment wrk gives it.
    let new: AttachResponse = call(&socket, &agent(dir.path(), None, None), TIMEOUT).unwrap();
    let id = new.session_id.clone().expect("a minted session id");
    assert_eq!(
        new.command,
        [
            "steam-run",
            "claude",
            "--session-id",
            id.as_str(),
            "--name",
            "main"
        ]
    );
    assert!(new.env.is_empty(), "{:?}", new.env);
    assert_eq!(new.cwd, Some(project(dir.path()).path));

    // Reopen: resume the recorded id.
    let resumed: AttachResponse =
        call(&socket, &agent(dir.path(), Some(&id), None), TIMEOUT).unwrap();
    assert_eq!(
        resumed.command,
        ["steam-run", "claude", "--resume", id.as_str()]
    );

    // Shell pane and session list.
    let shell = Request::Attach(AttachRequest {
        kind: AttachKind::Shell,
        ..match agent(dir.path(), None, None) {
            Request::Attach(a) => a,
            _ => unreachable!(),
        }
    });
    let shell: AttachResponse = call(&socket, &shell, TIMEOUT).unwrap();
    assert!(!shell.is_declined());
    let list = Request::ListSessions {
        project: project(dir.path()),
    };
    let _: ListResponse = call(&socket, &list, TIMEOUT).unwrap();
}

/// Relay mode: a hook line an agent sends to the plugin reaches the wrk socket
/// from its attach — and still does after the plugin daemon is restarted,
/// because the route travels in the agent's `WRK_TAB`, not in plugin memory.
#[test]
fn relay_mode_pushes_status_to_wrk_and_survives_a_plugin_restart() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("claude-ext.sock");
    let wrk_sock = dir.path().join("wrk.sock");
    let wrk = UnixListener::bind(&wrk_sock).unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in wrk.incoming().flatten() {
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).unwrap();
            let _ = tx.send(line.trim().to_string());
        }
    });

    let daemon = start(dir.path(), true);
    let hello: HelloResponse = call(&socket, &Request::Hello, TIMEOUT).unwrap();
    assert!(hello.capabilities.status);
    let attached: AttachResponse =
        call(&socket, &agent(dir.path(), None, Some(&wrk_sock)), TIMEOUT).unwrap();
    assert_eq!(
        attached.env.get("WRK_SOCK"),
        Some(&socket.display().to_string())
    );
    let agent_tab = attached
        .env
        .get("WRK_TAB")
        .cloned()
        .expect("a routed WRK_TAB");

    // What the agent's `wrk hook busy` sends, with the WRK_* it was given.
    let hook = |kind: &str| {
        let line = format!(r#"{{"cmd":"status","tab":"{agent_tab}","kind":"{kind}"}}"#);
        UnixStream::connect(&socket)
            .unwrap()
            .write_all(format!("{line}\n").as_bytes())
            .unwrap();
    };
    hook("busy");
    let relayed = rx.recv_timeout(TIMEOUT).expect("relayed");
    assert_eq!(relayed, r#"{"cmd":"status","tab":"tab3","kind":"busy"}"#);

    drop(daemon); // the plugin dies; wrk relaunches it on its next job
    let _daemon = start(dir.path(), true);
    hook("waiting");
    let relayed = rx.recv_timeout(TIMEOUT).expect("relayed after the restart");
    assert_eq!(relayed, r#"{"cmd":"status","tab":"tab3","kind":"waiting"}"#);
}
