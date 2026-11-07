# Writing a wrk session plugin

This guide is for anyone — human or coding agent — building a **session plugin**
for wrk: an external program that provides a wrk project's terminal sessions in
place of wrk's built-in local agent processes. The motivating case is mapping a
project onto a remote "box" (an EC2 instance your own service provisions) and
its tabs onto sessions on that box, without wrk ever knowing how boxes work.

It covers the contract, how wrk drives a plugin, a complete skeleton, status
reporting, and how to test a plugin *the way wrk will actually run it*. The Rust
examples here are compiled as doctests of this crate, so they track the real
types.

- Wire types: [`src/proto.rs`](src/proto.rs). wrk's client and launcher:
  [`src/client.rs`](src/client.rs).
- Reference plugin: [`crates/claude-plugin`](../claude-plugin) — wrk's own Claude
  support re-implemented as a plugin, with an end-to-end test
  ([`tests/e2e.rs`](../claude-plugin/tests/e2e.rs)) worth copying.
- User-facing docs: the "Session plugins" section of the root
  [`README.md`](../../README.md).

## The model in one paragraph

wrk is the **client**; your plugin is a **daemon** listening on a Unix domain
socket. For each pane wrk needs, it asks the plugin *what command to run*
(`attach`), and the plugin answers with an argv such as
`["ssh", "-t", "box-17", "tmux", "new-session", "-A", "-s", "main"]`. wrk spawns
that command locally in an ordinary terminal pane, so **the command is the
transport**. wrk's grid, scrollback, resize, mouse, and copy all just work, and
your plugin never handles terminal bytes. The plugin is a control plane only:
it decides which box and which session, and it can optionally list sessions,
provide the shell pane, report agent status, and hear about closed tabs.

## Setup: how a plugin is registered

In `~/.config/wrk/settings.toml`:

```toml
[plugin.boxes]
# How wrk starts the daemon when nothing is listening. Optional: omit it if you
# start the daemon yourself. Use absolute paths (see "Pitfalls").
command = ["/home/me/.local/bin/boxes-wrk-plugin", "--region", "us-west-2"]
# The control socket. Optional; the default is
# $XDG_RUNTIME_DIR/wrk/plugin/<name>.sock.
# socket = "/run/user/1000/boxes/control.sock"
startup_timeout = 10   # seconds a launched daemon has to start listening (default 10)
request_timeout = 30   # seconds wrk waits for any one reply (default 30)
```

A project opts in with `plugin = "<name>"`. The **add-project** dialog offers it
as a `provider` (cycled with ←/→ once any plugin is configured), or you can set
it by hand in `~/.config/wrk/projects.toml`. Per-project settings for the plugin go
in `[project.plugin_config]`, an opaque string→string table that wrk passes
through verbatim:

```toml
[[project]]
name = "svc"
path = "/home/me/svc"        # still a local directory: the cwd for the spawned commands
plugin = "boxes"

[project.plugin_config]
box = "i-0abc123"
```

`plugin_config` is hand-edited for now. The add-project dialog doesn't fill it,
so a plugin should give a clear error when a key it needs is missing.

## Transport and framing

- **One request per connection.** wrk connects, writes one request as a single
  line of JSON terminated by `\n`, reads one reply line, and closes. Never
  pretty-print a reply, because a newline inside it breaks the framing.
- **Liveness probes.** Before doing work, wrk checks the socket by connecting
  and hanging up immediately. Your server sees a connection that sends nothing
  (EOF). Ignore it quietly, without an error, a log line, or a crash.
- **Concurrency.** wrk sends requests concurrently: opening a project sends one
  `attach` per tab plus one for the shell, all at once. Several wrk instances can
  also share one daemon, because the socket path is per plugin, not per wrk.
  Handle each connection independently (a thread or task per connection), and
  never let a slow request block the accept loop.
- **Timeouts.** Every reply must arrive within `request_timeout`, otherwise the
  tab shows `no response after 30s waiting for a reply`.

## Replies: the `ok` envelope

A successful reply is the method's response object with `"ok": true` added. A
failure is `{"ok": false, "error": "<message>"}`, and wrk shows the message
**verbatim in the tab**, so write it for the user (say what's wrong and what to
change). In Rust, use `proto::ok_reply(&body)` and `proto::error_reply(msg)`.

Both sides ignore unknown fields, and every optional field has a default. An
older plugin keeps working with a newer wrk, and you may add fields of your own
to replies.

## Methods

Only `attach` is required. The others are optional and controlled by the
capabilities you advertise in `hello`.

### `hello` — handshake and capabilities

```json
{"method":"hello"}
```
```json
{"ok":true,"name":"boxes","version":"0.3.0","capabilities":{"list":true,"shell":true,"status":true,"close":true}}
```

| capability | when `true`, wrk… | when `false` (default) |
|---|---|---|
| `list` | sends `list_sessions` for the new-tab picker | the picker offers only "New session" |
| `shell` | asks you for the shell pane (`attach` with `kind: "shell"`) | runs a local `$SHELL` in the project directory |
| `status` | adds `wrk_sock` / `wrk_tab` to agent `attach`es, so you can push status | status comes from the agent's own hooks or an idle-time heuristic |
| `close` | sends `close` when a tab hosting one of your sessions is closed | sends nothing |

wrk asks **once per daemon**: it caches the answer until the socket stops
answering (a restarted daemon is asked again). **A `hello` reply wrk can't parse
silently counts as "no capabilities"**, so the picker, shell, and status quietly
fall back. Check your `hello` reply in a test (see Testing).

### `attach` — what to run for a pane

```json
{"method":"attach","project":{"name":"svc","path":"/home/me/svc","config":{"box":"i-0abc123"}},"session_id":"s-41","kind":"agent","fresh":false,"name":"main","wrk_sock":"/run/user/1000/wrk/sock/wrk-4242.sock","wrk_tab":"tab0"}
```
```json
{"ok":true,"command":["ssh","-t","box-17","tmux","new-session","-A","-s","s-41"],"session_id":"s-41"}
```

Request fields:

| field | meaning |
|---|---|
| `project` | `{name, path, config}`. `config` is the project's `plugin_config` (omitted when empty). |
| `kind` | `"agent"` for a tab in the main pane, `"shell"` for the shell pane (sent only with the `shell` capability). |
| `session_id` | The session this tab recorded last time, or absent. |
| `fresh` | `true` only when the user picked **New session** in the picker. |
| `name` | The tab's display name, as a hint for naming a new session. |
| `wrk_sock`, `wrk_tab` | Only with the `status` capability, and only for agent tabs. See Status. |

What `session_id` and `fresh` together ask for:

| `session_id` | `fresh` | means | sent when |
|---|---|---|---|
| `"s-41"` | `false` | re-attach session `s-41` | reopening a tab, wrk restarted, the user picked `s-41` in the picker |
| absent | `true` | create a new session | the picker's "New session" |
| absent | `false` | the project's default session; attach it or create it, your choice | the first open of a project with no tabs; a tab whose earlier attach returned no `session_id` |

Reply fields:

| field | meaning |
|---|---|
| `command` | The argv wrk spawns. It runs **locally**, under a pseudo-terminal. An empty `command` means "declined": for a shell, wrk falls back to a local shell; for an agent, the tab shows an error. |
| `env` | Extra environment variables, merged **over** wrk's own (see below). Optional. |
| `cwd` | **Local** working directory for the command. Optional; defaults to the project's `path`. |
| `session_id` | The session you attached or created. wrk saves it to `projects.toml` and sends it back on the next `attach` for this tab. That is how a reopened tab, or a restarted wrk, resumes the same session. |

Environment of the spawned command: wrk's own environment, plus

| variable | value |
|---|---|
| `TERM` | `xterm-256color` |
| `WRK_PROJECT` | the project name |
| `WRK_SOCK` | this wrk instance's IPC socket |
| `WRK_BIN` | the path of the running `wrk` binary |
| `WRK_TAB` | the tab's id (agent tabs only) |
| *your `env`* | overrides any of the above |

An agent running **on this machine** can therefore report status, open files
(`wrk view`), and run reviews exactly as in an internal project, without the
plugin doing anything. For a remote command, these variables stay on the local
side unless you forward them (for example `ssh -o SendEnv=…`, or
`env K=V cmd` in the remote command).

**The pane's process belongs to wrk.** It ends when the tab closes or wrk
exits. Anything that must outlive the pane, like the remote session itself, has
to live elsewhere: tmux or screen on the box, or your service. Attaching should
then reconnect to it.

### `list_sessions` — the new-tab picker (`list` capability)

```json
{"method":"list_sessions","project":{"name":"svc","path":"/home/me/svc","config":{"box":"i-0abc123"}}}
```
```json
{"ok":true,"sessions":[{"id":"s-41","name":"main","state":"running"},{"id":"s-40","state":"stopped"}]}
```

wrk opens the picker straight away with "loading sessions…" and fills it in when
your reply arrives, keeping your order (put the most relevant session first).
`name` is optional, and `state` (`running` / `stopped` / `unknown`) is
informational. Picking an entry sends `attach` with that `id` as `session_id`.
If the request fails, the picker shows your error and still offers "New
session".

### `close` — a tab was closed (`close` capability)

```json
{"method":"close","project":{"name":"svc","path":"/home/me/svc"},"session_id":"s-41"}
```

Reply `{"ok":true}`; the body is ignored. This is a best-effort notice: it is
sent only if your daemon is running (wrk never launches a plugin just to
deliver it), and failures are dropped. wrk also sends it when a tab is closed
while its `attach` is still in flight, for the session that `attach` returned,
so a session you created for a tab that no longer exists can be cleaned up.
Whether closing a tab should end the session is your decision. wrk's own
behaviour for local tabs is to stop the process but keep the session
resumable.

## How wrk drives a plugin

| the user… | wrk sends |
|---|---|
| opens the project (selects it in the sidebar) | `attach` (`kind: "agent"`) for every tab without a running pane, and `attach` (`kind: "shell"`) if the shell pane isn't running and you advertise `shell` |
| opens an empty project for the first time | one agent `attach` with no `session_id` and `fresh: false` |
| presses `Alt+n` (new tab) | `list_sessions` (with `list`), then the `attach` for the choice |
| closes a tab (`Alt+w`) | `close` (with `close`) |
| lets a pane's process exit | nothing. The tab shows "not running", and the next project open re-attaches it. |
| restarts wrk | on project open, an `attach` per saved tab with its recorded `session_id`, a **new** `wrk_tab`, and a **new** `wrk_sock` |

Before every request except `close`, wrk probes the socket. If nothing is
listening and `command` is set, it **launches** the daemon:

- **What runs:** `command`, with `WRK_PLUGIN_SOCK=<socket>` added to wrk's
  environment.
- **Where:** wrk's working directory, which you shouldn't rely on.
- **Input and output:** stdin is `/dev/null`. stdout and stderr are appended to
  `$XDG_RUNTIME_DIR/wrk/plugin/<name>.log`, after a
  `--- wrk: launching [...] ---` header.
- **Process group:** the daemon gets its own, so a hangup aimed at wrk's
  terminal doesn't reach it.
- **Stale socket:** a socket *file* at that path that nothing listens on is
  removed first.

Then it waits for the socket to start answering:

- If the process **exits first**, the tab shows its exit status and the last
  line it logged, at once.
- If it neither listens nor exits within `startup_timeout`, it is **killed**.
- After a failed launch, requests within 3 s get the same error without another
  launch, so one project open doesn't launch five times.

wrk **relaunches a daemon that has died** on the next request, and it **never
stops the daemon** when wrk exits. The daemon may serve other wrk instances, and
it may exit on its own when idle, since wrk will start it again.

## A complete plugin in Rust

Depend on this crate so your types match wrk's by construction:

```toml
[dependencies]
wrk-plugin = { git = "https://github.com/kinyoklion/wrk", tag = "<a wrk release tag>" }
anyhow = "1"
serde_json = "1"
```

The example below treats each project as an SSH host (`[project.plugin_config]
host = "…"`) and each tab as a tmux session on it. tmux keeps the session alive
on the host when the pane or wrk goes away, so reopening a tab re-attaches it.
Replace the bodies of `attach` and `list` with calls to your own service. The
rest is boilerplate you can keep.

```rust,no_run
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use wrk_plugin::client::SOCKET_ENV;
use wrk_plugin::proto::{
    self, AttachKind, AttachRequest, AttachResponse, Capabilities, HelloResponse, ListResponse,
    PluginSession, ProjectPayload, Request, SessionState,
};

fn main() -> Result<()> {
    // wrk names the socket to bind. Bind exactly that path.
    let socket = std::env::var_os(SOCKET_ENV).context("WRK_PLUGIN_SOCK is not set")?;
    let _ = std::fs::remove_file(&socket); // a leftover from a previous run
    let listener = UnixListener::bind(&socket).context("binding the control socket")?;
    // stdout and stderr go to wrk's plugin log, not to a terminal.
    eprintln!("ssh-tmux: listening on {}", socket.to_string_lossy());
    for stream in listener.incoming().flatten() {
        // wrk sends requests concurrently; a slow one must not hold up the rest.
        std::thread::spawn(move || serve(&stream));
    }
    Ok(())
}

/// One connection: one request line in, one reply line out.
fn serve(stream: &UnixStream) {
    let mut line = String::new();
    if BufReader::new(stream).read_line(&mut line).unwrap_or(0) == 0 {
        return; // wrk's liveness probe: it connects, then hangs up
    }
    let reply = match serde_json::from_str::<Request>(&line) {
        Ok(request) => handle(request).unwrap_or_else(|e| proto::error_reply(format!("{e:#}"))),
        // An unknown method or a malformed line gets an error reply, never a crash.
        Err(e) => proto::error_reply(format!("unsupported request: {e}")),
    };
    let mut stream = stream;
    if let Err(e) = stream.write_all(format!("{reply}\n").as_bytes()) {
        eprintln!("ssh-tmux: writing a reply failed: {e}");
    }
}

fn handle(request: Request) -> Result<String> {
    match request {
        Request::Hello => proto::ok_reply(&HelloResponse {
            name: "ssh-tmux".into(),
            version: "0.1.0".into(),
            // Advertise exactly what you implement.
            capabilities: Capabilities {
                list: true,
                shell: true,
                ..Default::default()
            },
        }),
        Request::ListSessions { project } => proto::ok_reply(&ListResponse {
            sessions: list(&project)?,
        }),
        Request::Attach(request) => proto::ok_reply(&attach(request)?),
        // Not advertised, so wrk never sends it, but answer anyway.
        Request::Close { .. } => proto::ok_reply(&serde_json::json!({})),
    }
}

/// The host a project lives on, from its `[project.plugin_config]`.
fn project_host(project: &ProjectPayload) -> Result<&str> {
    project.config.get("host").map(String::as_str).ok_or_else(|| {
        anyhow!(
            "set `host` under [project.plugin_config] for '{}' in projects.toml",
            project.name
        )
    })
}

fn attach(request: AttachRequest) -> Result<AttachResponse> {
    let host = project_host(&request.project)?.to_string();
    if request.kind == AttachKind::Shell {
        return Ok(AttachResponse {
            command: vec!["ssh".into(), "-t".into(), host],
            ..Default::default()
        });
    }
    let session = match (request.session_id, request.fresh) {
        (Some(id), _) => id,                       // reopen this tab's session
        (None, false) => "main".to_string(),       // the project's default session
        (None, true) => format!("wrk-{}", now_ms()), // the picker's "New session"
    };
    // ssh joins its arguments into one command line for the *remote* shell,
    // so quote anything you didn't generate yourself (listed names can be
    // arbitrary). `-A` attaches if the session exists, else creates it, which
    // makes a repeated attach harmless.
    let remote = format!("tmux new-session -A -s {}", shell_quote(&session));
    Ok(AttachResponse {
        command: vec!["ssh".into(), "-t".into(), host, remote],
        session_id: Some(session), // wrk records it and sends it back next time
        ..Default::default()        // no extra env; cwd = the project's local path
    })
}

/// The host's tmux sessions, for the new-tab picker.
fn list(project: &ProjectPayload) -> Result<Vec<PluginSession>> {
    let output = Command::new("ssh")
        // The daemon has no terminal: ssh must never prompt, and must give up
        // well within wrk's request_timeout.
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5"])
        .arg(project_host(project)?)
        .arg("tmux list-sessions -F '#{session_name}'")
        .output()
        .context("running ssh")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("no server running") {
            return Ok(Vec::new()); // nothing started on this host yet
        }
        return Err(anyhow!("listing tmux sessions failed: {}", stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|name| !name.is_empty())
        .map(|name| PluginSession {
            id: name.to_string(),
            name: None,
            state: SessionState::Running,
        })
        .collect())
}

/// `'…'`-quote `s` for a POSIX shell.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
}
```

Register it with `command = ["/abs/path/to/ssh-tmux"]` under
`[plugin.ssh-tmux]`.

**Other languages.** Nothing requires Rust. Any program that can listen on a
Unix socket and speak the JSON above is a plugin. Build the replies from the
examples in this guide, and **make the launch command something that runs on
the host as wrk sees it** (see Pitfalls). That is where interpreter-based
plugins usually go wrong.

## Status: keeping the sidebar dot accurate

wrk's sidebar shows a dot per tab: busy, stopped (idle), or waiting on the user.
It learns these from status lines sent to its socket. A plugin tab has three
possible sources:

1. **The agent's own hooks.** If the agent runs on this machine with wrk's hooks
   installed (`wrk install-hooks`), it reports through the `WRK_SOCK` / `WRK_TAB`
   wrk put in its environment. The plugin does nothing.
2. **The plugin pushes status.** For agents that can't reach wrk, such as one on
   a remote box. Advertise the `status` capability. Every agent `attach` then
   carries `wrk_sock` (wrk's socket) and `wrk_tab` (the tab's id). To report a
   state change, connect to `wrk_sock` and write one line, the same one
   `wrk hook` sends. wrk sends no reply.
3. **Neither.** wrk falls back to a heuristic: no terminal output for a moment
   reads as "waiting".

```rust,no_run
use std::io::Write;
use std::os::unix::net::UnixStream;

/// Tell wrk that tab `wrk_tab` is now `kind`: the line `wrk hook <kind>` sends.
/// Fire-and-forget; wrk sends no reply.
fn push_status(wrk_sock: &str, wrk_tab: &str, kind: &str) -> std::io::Result<()> {
    let line = serde_json::json!({ "cmd": "status", "tab": wrk_tab, "kind": kind });
    UnixStream::connect(wrk_sock)?.write_all(format!("{line}\n").as_bytes())
}
```

| `kind` | meaning |
|---|---|
| `busy` | the agent is working (the user submitted a prompt) |
| `stopped` | the agent finished its turn and is idle |
| `waiting` | the agent is blocked on the user (a permission prompt, a question) |
| `subagent-start`, `subagent-stop` | a sub-agent started or finished (a counter) |
| `session` | the agent switched sessions; send the new `"session_id"` with it |

Rules:

- **Key your routing on what the latest `attach` sent.** `wrk_tab` and
  `wrk_sock` change every time wrk restarts, and one session can be attached by
  more than one tab or wrk instance. Update the mapping on every `attach`.
- **Leave out `session_id` unless the session really changed.** wrk adopts a new
  id (and may rename the tab), so send it only with `session`, when the agent
  moved to a different session that a later `attach` should resume.
- **A refused connection means that wrk is gone.** Drop the route; don't retry.
- **Your daemon may restart while wrk runs.** wrk relaunches it but does not
  re-`attach` running tabs, so keep routes somewhere that survives a restart.
  The reference plugin's `--relay-status` mode encodes the route in the agent's
  own environment for this reason (`WRK_TAB=<tab>@<wrk socket>`). A status
  change that happens while your daemon is down is lost.

How the plugin learns its agents' states is up to you: your service's API, or
hooks on the box that call back to the plugin.

## Pitfalls

These are the ways plugins actually fail. Most of them hit the first version
of the reference plugin.

- **The launch command must run where wrk runs it, not just in your dev shell.**
  wrk executes `command` with its own environment and `PATH`. An interpreter on
  your shell's `PATH` (a `python3` inside a nix shell, a venv, a container, or a
  sandbox) may not exist for wrk. Then the daemon exits with status 127 before
  it ever listens. Use absolute paths, prefer a compiled binary, and test the
  launch with wrk's launcher (see Testing).
- **Unix socket paths are limited to about 108 bytes.** A deep
  `XDG_RUNTIME_DIR` or a test tempdir can exceed it, and the bind fails. Keep
  socket paths short.
- **Bind exactly `WRK_PLUGIN_SOCK`.** wrk waits for that path. If you also start
  the daemon by hand, remove a stale socket file before binding, as the
  skeleton does.
- **Ignore empty connections.** These are liveness probes. Logging each one
  floods the log, and crashing on one makes the plugin look dead.
- **Never block the accept loop.** Use one thread or task per connection. A
  plugin that handles requests one at a time makes every tab wait on the
  slowest request.
- **Keep `attach` fast.** If getting a box ready takes minutes, don't make
  `attach` wait (wrk gives up after `request_timeout`). Return a command that
  does the waiting itself, for example `["boxctl", "connect", "--wait",
  "i-0abc123"]`. The user then watches progress in the pane, can read errors
  there, and can close the tab to cancel. Raise `request_timeout` only for
  work that must finish before you know what to run.
- **Make `attach` idempotent.** wrk re-attaches on every project open where a
  pane isn't running, after every wrk restart, and when two tabs or two wrk
  instances pick the same session. Attaching the same `session_id` twice must
  reconnect, not create a second box or session.
- **Quote what you put into remote command lines.** ssh joins its arguments
  into one string for the remote shell. A session name containing spaces or
  `;` breaks the command, or worse. Prefer argv everywhere else, which is what
  `command` already is.
- **The daemon has no terminal.** Its stdin is `/dev/null`, so anything it runs
  must not prompt (`ssh -o BatchMode=yes`). The *pane's* command does have a
  terminal, so an interactive prompt there (a password, an MFA code) is fine.
- **Logs are appended and not rotated.** Write to stderr in moderation, and
  never log secrets.
- **Don't assume a single wrk.** Several instances can share one daemon. Don't
  exit when one of them goes quiet, unless an idle exit is what you want.

## Testing

Test at three levels. Each one catches a different class of bug.

1. **Handlers, as plain functions.** Build an `AttachRequest`, call your
   `attach`, and assert the command. Check that the `hello` reply parses:
   `proto::parse_reply::<HelloResponse>(&reply)`.
2. **The wire format, in memory.** `UnixStream::pair()` gives you two connected
   ends. Write a request line into one, call `serve` on the other, and read the
   reply. Also check that an empty connection is ignored. See the reference
   plugin's `serve_answers_requests_and_relays_hook_lines_without_replying`.
3. **The launch, exactly as wrk does it.** This is the level that catches the
   "works in my shell" failures. In a Cargo integration test,
   `env!("CARGO_BIN_EXE_<your-bin>")` is the path of your built binary:

```rust,no_run
use std::path::Path;
use std::time::Duration;

use wrk_plugin::client::{call, launch};
use wrk_plugin::proto::{HelloResponse, Request};

/// Start the plugin with wrk's own launcher and the same `command` you put in
/// settings.toml, then talk to it over its socket.
fn launched_like_wrk(command: &[String], dir: &Path) -> anyhow::Result<()> {
    let socket = dir.join("p.sock"); // keep it short: ~108-byte limit
    let mut daemon = launch(command, &socket, &dir.join("p.log"), Duration::from_secs(10))?;
    let hello: HelloResponse = call(&socket, &Request::Hello, Duration::from_secs(5))?;
    assert_eq!(hello.name, "ssh-tmux");
    // … then an `attach` for each case in the attach table above …
    daemon.kill()?;
    daemon.wait()?;
    Ok(())
}
```

`launch` fails the same way wrk would: it reports the exit status and the last
log line if the daemon dies, and kills it if it never listens.

**Poking a running daemon by hand.** `socat` is a quick way to send a single
request:

```sh
sock=$XDG_RUNTIME_DIR/wrk/plugin/ssh-tmux.sock
printf '%s\n' '{"method":"hello"}' | socat -t 30 - UNIX-CONNECT:"$sock"
tail -n 20 "$XDG_RUNTIME_DIR/wrk/plugin/ssh-tmux.log"
```

**In a real wrk, without touching your own.** wrk reads its config from
`$XDG_CONFIG_HOME/wrk/` and keeps sockets and logs under `$XDG_RUNTIME_DIR/wrk/`,
so a scratch pair of directories gives you a separate instance. Keep the runtime
path short:

```sh
mkdir -p /tmp/wt/config/wrk /tmp/wt/run && chmod 700 /tmp/wt/run
# write /tmp/wt/config/wrk/settings.toml ([plugin.…]) and projects.toml (plugin = "…")
XDG_CONFIG_HOME=/tmp/wt/config XDG_RUNTIME_DIR=/tmp/wt/run wrk
```

A coding agent can drive that headlessly with tmux:

```sh
tmux new-session -d -s wt -x 160 -y 45 'XDG_CONFIG_HOME=/tmp/wt/config XDG_RUNTIME_DIR=/tmp/wt/run wrk'
tmux send-keys -t wt Enter
tmux capture-pane -p -t wt
```

The first key sent right after wrk starts can be swallowed, so allow a moment
and re-check. Then go through the user's actions:

- open the project;
- add a tab with `Alt+n`, both "New session" and a listed session;
- close a tab;
- end a pane's process, then reopen the project (it should re-attach);
- quit and restart wrk (each tab should resume its recorded session);
- kill the daemon, then reopen (wrk should relaunch it);
- break the launch command (the tab should explain why at once).

## Errors wrk shows, and what they mean

| message in the tab | cause |
|---|---|
| `there is no [plugin.<name>] table in settings.toml` | The project names a plugin that isn't registered. |
| ``nothing is listening on <socket> and no `command` is configured to start it`` | The daemon isn't running and wrk wasn't told how to start it. |
| ``starting the plugin failed: could not run `<program>`: …`` | `command[0]` doesn't exist or isn't executable *for wrk*. |
| `starting the plugin failed: the process exited (exit status: N) before listening: <last log line> (log: …)` | The daemon died during startup. The log has the rest. |
| `starting the plugin failed: the process did not listen on <socket> within Ns and was stopped` | It never bound the socket: wrong path, or startup is too slow for `startup_timeout`. |
| `no response after Ns waiting for a reply` | A request took longer than `request_timeout`. |
| `the plugin closed the connection without replying` | The handler crashed or returned without writing a line. |
| `malformed reply (…)` / `unexpected reply (…)` | The reply isn't one line of JSON, or doesn't match the method's response. |
| `the plugin returned no command to run` | An agent `attach` returned an empty `command`. |
| anything else | Your own `{"ok": false, "error": …}` message. |

These appear in an agent tab's pane. A failed **shell** `attach` doesn't leave
the shell pane empty: wrk starts a local shell instead and shows
`plugin '<name>': using a local shell (<error>)` in the status bar. The same
errors appear in the new-tab picker as `couldn't list sessions: <error>`.

## Before you call it done

- [ ] `hello` parses with `parse_reply::<HelloResponse>` and advertises exactly
      the capabilities you implement.
- [ ] The `command` in `settings.toml` uses absolute paths, and an integration
      test starts it with `wrk_plugin::client::launch`.
- [ ] Every request gets exactly one single-line reply within `request_timeout`,
      and errors are messages a user can act on.
- [ ] Empty connections (probes) are ignored quietly, and requests are handled
      concurrently.
- [ ] `attach` is idempotent for a given `session_id`, follows the
      `session_id` / `fresh` table, and returns `session_id` for anything it
      creates.
- [ ] Anything that must survive a closed pane or a wrk restart lives outside
      the pane's process.
- [ ] Values put into remote shell command lines are quoted.
- [ ] With `status`: routes are updated on every `attach`, pushes to a gone wrk
      are dropped, and you've decided what happens if the daemon restarts.
- [ ] You've walked through the "In a real wrk" checklist in a scratch instance.
