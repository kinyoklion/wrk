//! `wrk-claude-plugin` — a reference wrk session plugin that externalizes wrk's
//! own built-in Claude support over the plugin protocol.
//!
//! For a project bound to it, it does what wrk does internally for a Claude
//! project:
//!
//! - lists the project's on-disk Claude sessions (`~/.claude/projects/<key>`),
//! - starts a new session as `claude --session-id <uuid> --name <tab>` and
//!   returns the minted id so wrk records it, or reopens one with
//!   `claude --resume <id>`,
//! - serves a local `$SHELL` for the shell pane.
//!
//! Claude runs locally, so it can reach wrk directly: wrk already exports
//! `WRK_SOCK` / `WRK_TAB` / `WRK_BIN` into the command a plugin returns, and the
//! agent's own wrk hooks report status, `wrk view` and `wrk review` exactly as
//! in an internal project. The plugin leaves that environment alone.
//!
//! With `--relay-status` it instead behaves like a plugin whose agents *can't*
//! reach wrk (e.g. on a remote box): it advertises the `status` capability,
//! points the agent's `WRK_SOCK` at itself, and relays the agent's status lines
//! to wrk — exercising plugin-pushed status end to end. The destination rides in
//! the agent's `WRK_TAB` (`<tab>@<wrk socket>`), so relaying is stateless: it
//! survives a plugin restart and serves any number of wrk instances. As on a
//! real remote box, wrk's local-only commands that identify the instance from
//! `WRK_SOCK` (`wrk review end`) aren't available to agents in this mode.
//!
//! wrk launches it with the socket to bind in `WRK_PLUGIN_SOCK`; its arguments
//! are an optional `--relay-status` and then the Claude launch command (default
//! `claude`):
//!
//! ```toml
//! [plugin.claude-ext]
//! command = ["/path/to/wrk-claude-plugin", "steam-run", "claude"]
//! ```
//!
//! Diagnostics go to stderr, which wrk captures in the plugin's log.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result};
use wrk_plugin::client::SOCKET_ENV;
use wrk_plugin::proto::{
    self, AttachKind, AttachRequest, AttachResponse, Capabilities, HelloResponse, ListResponse,
    PluginSession, Request, SessionState,
};

/// Separates the real tab id from the wrk socket in a relayed `WRK_TAB`.
const ROUTE_SEP: char = '@';

fn main() -> Result<()> {
    let socket = PathBuf::from(std::env::var_os(SOCKET_ENV).with_context(|| {
        format!("{SOCKET_ENV} is not set (wrk sets it when it launches the plugin)")
    })?);
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let relay_status = args.first().is_some_and(|a| a == "--relay-status");
    if relay_status {
        args.remove(0);
    }
    if args.first().is_some_and(|a| a == "--") {
        args.remove(0);
    }
    let claude = if args.is_empty() {
        vec!["claude".to_string()]
    } else {
        args
    };
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let plugin = Arc::new(Plugin {
        claude,
        shell: vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())],
        socket: socket.clone(),
        claude_projects: home.join(".claude/projects"),
        relay_status,
    });

    let _ = std::fs::remove_file(&socket);
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    eprintln!(
        "wrk-claude-plugin: listening on {} (claude: {:?}, relay status: {relay_status})",
        socket.display(),
        plugin.claude
    );
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let plugin = Arc::clone(&plugin);
                std::thread::spawn(move || plugin.serve(&stream));
            }
            Err(e) => eprintln!("wrk-claude-plugin: accept failed: {e}"),
        }
    }
    Ok(())
}

struct Plugin {
    /// The Claude launch command (+ wrapper args) sessions are started with.
    claude: Vec<String>,
    /// The shell-pane command.
    shell: Vec<String>,
    /// Our own control socket — in relay mode, also where agents report.
    socket: PathBuf,
    /// Claude's per-project history root (`~/.claude/projects`).
    claude_projects: PathBuf,
    /// `--relay-status`: push agents' status ourselves (see the module docs).
    relay_status: bool,
}

impl Plugin {
    /// Handle one connection: a protocol request from wrk (answered), or — in
    /// relay mode — a wrk IPC line from an agent's hook (relayed; the sender
    /// expects no reply).
    fn serve(&self, stream: &UnixStream) {
        let mut line = String::new();
        if BufReader::new(stream).read_line(&mut line).unwrap_or(0) == 0 {
            return; // a liveness probe: connect, then hang up
        }
        let reply = match serde_json::from_str::<serde_json::Value>(line.trim()) {
            Ok(value) if value.get("cmd").is_some() => {
                self.relay(value);
                return;
            }
            Ok(value) => match serde_json::from_value::<Request>(value) {
                Ok(request) => self
                    .handle(request)
                    .unwrap_or_else(|e| proto::error_reply(format!("{e:#}"))),
                Err(e) => proto::error_reply(format!("unsupported request: {e}")),
            },
            Err(e) => proto::error_reply(format!("malformed request: {e}")),
        };
        let mut stream = stream;
        if let Err(e) = stream.write_all(format!("{reply}\n").as_bytes()) {
            eprintln!("wrk-claude-plugin: writing a reply failed: {e}");
        }
    }

    fn handle(&self, request: Request) -> Result<String> {
        match request {
            Request::Hello => proto::ok_reply(&HelloResponse {
                name: "claude-ext".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                capabilities: Capabilities {
                    list: true,
                    shell: true,
                    status: self.relay_status,
                    close: true,
                },
            }),
            Request::ListSessions { project } => proto::ok_reply(&ListResponse {
                sessions: self.sessions(&project.path),
            }),
            Request::Attach(request) => proto::ok_reply(&self.attach(request)),
            // Claude sessions live on disk; a closed tab leaves nothing to tear down.
            Request::Close { .. } => proto::ok_reply(&serde_json::json!({})),
        }
    }

    /// The command for one pane — the same flags wrk's internal Claude path uses.
    fn attach(&self, request: AttachRequest) -> AttachResponse {
        let cwd = Some(request.project.path.clone());
        if request.kind == AttachKind::Shell {
            return AttachResponse {
                command: self.shell.clone(),
                cwd,
                ..Default::default()
            };
        }
        let mut env = BTreeMap::new();
        if self.relay_status
            && let (Some(wrk_sock), Some(wrk_tab)) = (&request.wrk_sock, &request.wrk_tab)
        {
            env.insert(
                "WRK_SOCK".to_string(),
                self.socket.to_string_lossy().into_owned(),
            );
            env.insert(
                "WRK_TAB".to_string(),
                format!("{wrk_tab}{ROUTE_SEP}{wrk_sock}"),
            );
        }
        let mut command = self.claude.clone();
        let session_id = match request.session_id {
            Some(id) => {
                command.extend(["--resume".to_string(), id.clone()]);
                id
            }
            None => {
                let id = uuid::Uuid::new_v4().to_string();
                let name = if request.name.is_empty() {
                    request.project.name
                } else {
                    request.name
                };
                command.extend([
                    "--session-id".to_string(),
                    id.clone(),
                    "--name".to_string(),
                    name,
                ]);
                id
            }
        };
        AttachResponse {
            command,
            env,
            cwd,
            session_id: Some(session_id),
        }
    }

    /// The project's Claude sessions, newest first — the discovery wrk does for
    /// an internal project (`<projects>/<path with / → ->/<id>.jsonl`).
    fn sessions(&self, project_path: &str) -> Vec<PluginSession> {
        let dir = self.claude_projects.join(project_path.replace('/', "-"));
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut found: Vec<(SystemTime, String)> = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    return None;
                }
                let id = path.file_stem()?.to_str()?.to_owned();
                if id == "memory" {
                    return None;
                }
                let modified = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                Some((modified, id))
            })
            .collect();
        found.sort_by_key(|(modified, _)| Reverse(*modified));
        found
            .into_iter()
            .map(|(_, id)| PluginSession {
                id,
                name: None,
                state: SessionState::Unknown,
            })
            .collect()
    }

    /// Relay mode: deliver an agent's wrk IPC line to the wrk instance named in
    /// its routed tab id, with the real tab id restored.
    fn relay(&self, mut line: serde_json::Value) {
        if !self.relay_status {
            eprintln!("wrk-claude-plugin: ignored a wrk IPC line (not in relay mode): {line}");
            return;
        }
        let Some(target) = route(&mut line) else {
            eprintln!("wrk-claude-plugin: dropped a wrk IPC line with no routable tab: {line}");
            return;
        };
        let sent = UnixStream::connect(&target)
            .and_then(|mut s| s.write_all(format!("{line}\n").as_bytes()));
        if let Err(e) = sent {
            eprintln!("wrk-claude-plugin: relaying to {target} failed: {e}");
        }
    }
}

/// Split a relayed line's routed `tab` (`<tab>@<wrk socket>`) in place —
/// restoring the real tab id — and return the wrk socket to deliver it to.
fn route(line: &mut serde_json::Value) -> Option<String> {
    let (tab, target) = line.get("tab")?.as_str()?.split_once(ROUTE_SEP)?;
    let (tab, target) = (tab.to_owned(), target.to_owned());
    line["tab"] = tab.into();
    Some(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::path::Path;
    use std::time::Duration;
    use wrk_plugin::proto::ProjectPayload;

    fn plugin(dir: &Path, relay_status: bool) -> Plugin {
        Plugin {
            claude: vec!["steam-run".into(), "claude".into()],
            shell: vec!["/bin/zsh".into()],
            socket: dir.join("plugin.sock"),
            claude_projects: dir.join("projects"),
            relay_status,
        }
    }

    /// An attach request as wrk sends it to a plugin with the `status`
    /// capability (with `wrk_sock`) or without.
    fn attach(kind: AttachKind, session_id: Option<&str>, wrk_sock: Option<&str>) -> AttachRequest {
        AttachRequest {
            project: ProjectPayload {
                name: "svc".into(),
                path: "/home/me/svc".into(),
                config: BTreeMap::new(),
            },
            session_id: session_id.map(str::to_owned),
            kind,
            fresh: session_id.is_none(),
            name: "main".into(),
            wrk_sock: wrk_sock.map(str::to_owned),
            wrk_tab: Some("tab0".into()),
        }
    }

    #[test]
    fn a_new_session_mints_an_id_and_names_the_tab_like_wrk_does() {
        let dir = tempfile::tempdir().unwrap();
        let resp = plugin(dir.path(), false).attach(attach(AttachKind::Agent, None, None));
        let id = resp.session_id.clone().unwrap();
        assert!(uuid::Uuid::parse_str(&id).is_ok(), "{id}");
        assert_eq!(
            resp.command,
            [
                "steam-run",
                "claude",
                "--session-id",
                id.as_str(),
                "--name",
                "main"
            ]
        );
        assert_eq!(resp.cwd.as_deref(), Some("/home/me/svc"));
    }

    #[test]
    fn a_known_session_is_resumed() {
        let dir = tempfile::tempdir().unwrap();
        let resp = plugin(dir.path(), false).attach(attach(AttachKind::Agent, Some("s-1"), None));
        assert_eq!(resp.command, ["steam-run", "claude", "--resume", "s-1"]);
        assert_eq!(resp.session_id.as_deref(), Some("s-1"));
    }

    /// By default the agent keeps the WRK_SOCK / WRK_TAB wrk gave it, so its
    /// hooks — and `wrk view` / `wrk review` — reach wrk directly.
    #[test]
    fn by_default_the_agents_wrk_environment_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let p = plugin(dir.path(), false);
        let resp = p.attach(attach(AttachKind::Agent, None, Some("/run/wrk.sock")));
        assert!(resp.env.is_empty(), "{:?}", resp.env);
        let hello: HelloResponse = proto::parse_reply(&p.handle(Request::Hello).unwrap()).unwrap();
        assert!(!hello.capabilities.status, "it does not push status itself");
    }

    #[test]
    fn relay_mode_routes_the_agents_hooks_through_the_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let p = plugin(dir.path(), true);
        let resp = p.attach(attach(AttachKind::Agent, None, Some("/run/wrk.sock")));
        assert_eq!(
            resp.env.get("WRK_SOCK").map(String::as_str),
            Some(p.socket.to_str().unwrap())
        );
        assert_eq!(
            resp.env.get("WRK_TAB").map(String::as_str),
            Some("tab0@/run/wrk.sock")
        );
        let hello: HelloResponse = proto::parse_reply(&p.handle(Request::Hello).unwrap()).unwrap();
        assert!(hello.capabilities.status);
    }

    #[test]
    fn a_routed_tab_is_split_into_the_real_tab_and_its_wrk_socket() {
        let mut line =
            serde_json::json!({"cmd": "status", "tab": "tab3@/run/a@b/wrk-1.sock", "kind": "busy"});
        assert_eq!(route(&mut line).as_deref(), Some("/run/a@b/wrk-1.sock"));
        assert_eq!(
            line,
            serde_json::json!({"cmd": "status", "tab": "tab3", "kind": "busy"})
        );
        assert!(route(&mut serde_json::json!({"cmd": "status", "tab": "tab3"})).is_none());
        assert!(route(&mut serde_json::json!({"cmd": "open", "path": "/x"})).is_none());
    }

    #[test]
    fn the_shell_pane_gets_a_local_shell_in_the_project_dir() {
        let dir = tempfile::tempdir().unwrap();
        let resp =
            plugin(dir.path(), true).attach(attach(AttachKind::Shell, None, Some("/run/wrk.sock")));
        assert_eq!(resp.command, ["/bin/zsh"]);
        assert_eq!(resp.cwd.as_deref(), Some("/home/me/svc"));
        assert!(resp.session_id.is_none() && resp.env.is_empty());
    }

    #[test]
    fn sessions_are_listed_newest_first_skipping_non_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let p = plugin(dir.path(), false);
        let sessions = p.claude_projects.join("-home-me-svc");
        std::fs::create_dir_all(&sessions).unwrap();
        let now = SystemTime::now();
        for (name, age) in [
            ("old.jsonl", 60),
            ("new.jsonl", 0),
            ("memory.jsonl", 0),
            ("notes.txt", 0),
        ] {
            let file = File::create(sessions.join(name)).unwrap();
            file.set_modified(now - Duration::from_secs(age)).unwrap();
        }
        let ids: Vec<String> = p
            .sessions("/home/me/svc")
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, ["new", "old"]);
        assert!(p.sessions("/nowhere").is_empty());
    }

    #[test]
    fn serve_answers_requests_and_relays_hook_lines_without_replying() {
        let dir = tempfile::tempdir().unwrap();
        let p = plugin(dir.path(), true);

        // A protocol request is answered.
        let (ours, theirs) = UnixStream::pair().unwrap();
        (&ours).write_all(b"{\"method\":\"hello\"}\n").unwrap();
        p.serve(&theirs);
        let mut reply = String::new();
        BufReader::new(&ours).read_line(&mut reply).unwrap();
        let hello: HelloResponse = proto::parse_reply(&reply).unwrap();
        assert!(hello.capabilities.status && hello.capabilities.list);

        // A hook line is delivered to the wrk socket named in its tab.
        let wrk_sock = dir.path().join("wrk.sock");
        let wrk = UnixListener::bind(&wrk_sock).unwrap();
        let (ours, theirs) = UnixStream::pair().unwrap();
        let hook = format!(
            r#"{{"cmd":"status","tab":"tab0@{}","kind":"busy"}}"#,
            wrk_sock.display()
        );
        (&ours).write_all(format!("{hook}\n").as_bytes()).unwrap();
        p.serve(&theirs);
        let (relayed, _) = wrk.accept().unwrap();
        let mut got = String::new();
        BufReader::new(&relayed).read_line(&mut got).unwrap();
        assert_eq!(got.trim(), r#"{"cmd":"status","tab":"tab0","kind":"busy"}"#);
    }
}
