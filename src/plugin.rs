//! wrk's host for session plugins.
//!
//! The wire protocol, request/reply client, and daemon launcher live in the
//! `wrk-plugin` crate (shared with plugins written in Rust). This module runs
//! them for the TUI without ever blocking it: the app [`submit`](PluginHost::submit)s
//! a [`Job`], a worker thread connects to the plugin — launching it if nothing
//! is listening — and performs the job, and the outcome comes back as a
//! [`PluginEvent`] that the event loop drains every tick. A slow, hung, or broken
//! plugin costs a "connecting…" placeholder, never a frozen UI.
//!
//! Each plugin's [`PluginState`] is shared by its workers behind a mutex, so:
//! - launches are serialized — concurrent jobs wait for one launch and reuse it;
//! - a daemon that has died since the last job is relaunched (self-healing);
//! - after a failed launch, jobs within [`RETRY_BACKOFF`] fail fast with the same
//!   error instead of each relaunching (one project open queues a job per tab
//!   plus one for the shell).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use serde::de::IgnoredAny;
use wrk_plugin::client;
use wrk_plugin::proto::{
    AttachKind, AttachRequest, Capabilities, HelloResponse, ListResponse, ProjectPayload, Request,
};
pub use wrk_plugin::proto::{AttachResponse, PluginSession};

use crate::settings::PluginConfig;
use crate::store::Project;

/// How long a failed launch is remembered: jobs in this window report the same
/// failure immediately rather than relaunching.
const RETRY_BACKOFF: Duration = Duration::from_secs(3);

/// The protocol's view of a project.
pub fn payload(project: &Project) -> ProjectPayload {
    ProjectPayload {
        name: project.name.clone(),
        path: project.path.to_string_lossy().into_owned(),
        config: project.plugin_config.clone(),
    }
}

/// Where default plugin sockets and launch logs live: `<runtime>/wrk/plugin/`.
pub fn default_base_dir() -> PathBuf {
    crate::status::runtime_dir().join("plugin")
}

/// Work for a plugin, run off the UI thread.
#[derive(Debug)]
pub enum Job {
    /// Resolve the command for agent tab `tab` (its `WRK_TAB` id).
    Attach {
        project: ProjectPayload,
        tab: String,
        session_id: Option<String>,
        fresh: bool,
        name: String,
    },
    /// Resolve the project's shell pane (`None` result ⇒ use a local shell).
    Shell { project: ProjectPayload },
    /// List the project's sessions for the new-tab picker.
    List { project: ProjectPayload },
    /// Tell the plugin a tab closed. Fire-and-forget, and never launches it.
    Close {
        project: ProjectPayload,
        session_id: String,
    },
}

/// The outcome of a [`Job`], for the event loop. Errors are for display (the
/// app prefixes the plugin name).
#[derive(Debug)]
pub enum PluginEvent {
    Attached {
        plugin: String,
        project: String,
        tab: String,
        result: Result<AttachResponse>,
    },
    Shell {
        plugin: String,
        project: String,
        result: Result<Option<AttachResponse>>,
    },
    Sessions {
        plugin: String,
        project: String,
        result: Result<Vec<PluginSession>>,
    },
}

/// Runs plugin jobs on worker threads and collects their results.
pub struct PluginHost {
    configs: BTreeMap<String, PluginConfig>,
    base_dir: PathBuf,
    /// wrk's IPC socket, offered to plugins that push status.
    wrk_sock: Option<String>,
    states: HashMap<String, Arc<Mutex<PluginState>>>,
    events_tx: Sender<PluginEvent>,
    events: Receiver<PluginEvent>,
}

impl PluginHost {
    pub fn new(configs: BTreeMap<String, PluginConfig>, base_dir: PathBuf) -> Self {
        let (events_tx, events) = channel();
        Self {
            configs,
            base_dir,
            wrk_sock: None,
            states: HashMap::new(),
            events_tx,
            events,
        }
    }

    /// Set wrk's IPC socket (known only once it has been bound).
    pub fn set_wrk_sock(&mut self, sock: Option<&Path>) {
        self.wrk_sock = sock.map(|p| p.to_string_lossy().into_owned());
    }

    /// Run `job` against `plugin` on a worker thread; its result (if the job
    /// has one) arrives later via [`try_recv`](Self::try_recv).
    pub fn submit(&mut self, plugin: &str, job: Job) {
        let worker = self.worker(plugin);
        let tx = self.events_tx.clone();
        std::thread::spawn(move || {
            if let Some(event) = worker.run(job) {
                let _ = tx.send(event); // receiver gone ⇒ the app is exiting
            }
        });
    }

    /// The next finished job's result, if any.
    pub fn try_recv(&self) -> Option<PluginEvent> {
        self.events.try_recv().ok()
    }

    fn worker(&mut self, plugin: &str) -> Worker {
        let config = self.configs.get(plugin).cloned();
        let state = self
            .states
            .entry(plugin.to_string())
            .or_insert_with(|| {
                let socket = socket_path(&self.base_dir, plugin, config.as_ref());
                Arc::new(Mutex::new(PluginState::new(socket)))
            })
            .clone();
        Worker {
            plugin: plugin.to_string(),
            config,
            state,
            wrk_sock: self.wrk_sock.clone(),
            log: self.base_dir.join(format!("{plugin}.log")),
        }
    }
}

/// The plugin's control socket: the configured `socket` (with `~` expanded),
/// else `<base_dir>/<name>.sock`.
fn socket_path(base_dir: &Path, name: &str, config: Option<&PluginConfig>) -> PathBuf {
    match config.and_then(|c| c.socket.as_deref()) {
        Some(explicit) => crate::expand_tilde(explicit),
        None => base_dir.join(format!("{name}.sock")),
    }
}

/// What wrk knows about one plugin, shared by its workers.
struct PluginState {
    socket: PathBuf,
    /// From `hello`; `None` until a handshake with the current daemon.
    caps: Option<Capabilities>,
    /// The daemon this wrk launched, kept so it can be reaped when it exits.
    /// Dropping it does not stop the daemon, which may outlive wrk.
    daemon: Option<Child>,
    /// The last launch failure and when it happened, for [`RETRY_BACKOFF`].
    failure: Option<(Instant, String)>,
}

impl PluginState {
    fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            caps: None,
            daemon: None,
            failure: None,
        }
    }

    /// Forget (and reap) a launched daemon that has exited.
    fn reap(&mut self) {
        if let Some(daemon) = self.daemon.as_mut()
            && !matches!(daemon.try_wait(), Ok(None))
        {
            self.daemon = None;
        }
    }
}

/// Everything one job needs, moved onto its thread.
struct Worker {
    plugin: String,
    config: Option<PluginConfig>,
    state: Arc<Mutex<PluginState>>,
    wrk_sock: Option<String>,
    log: PathBuf,
}

/// A reachable plugin, ready for requests.
struct Connection {
    socket: PathBuf,
    caps: Capabilities,
    timeout: Duration,
}

impl Worker {
    fn run(&self, job: Job) -> Option<PluginEvent> {
        let plugin = self.plugin.clone();
        Some(match job {
            Job::Attach {
                project,
                tab,
                session_id,
                fresh,
                name,
            } => {
                let project_name = project.name.clone();
                let result = self.connect(true).and_then(|conn| {
                    let request = Request::Attach(AttachRequest {
                        project,
                        session_id,
                        kind: AttachKind::Agent,
                        fresh,
                        name,
                        wrk_sock: self.wrk_sock.clone().filter(|_| conn.caps.status),
                        wrk_tab: Some(tab.clone()),
                    });
                    let resp: AttachResponse = client::call(&conn.socket, &request, conn.timeout)?;
                    if resp.is_declined() {
                        return Err(anyhow!("the plugin returned no command to run"));
                    }
                    Ok(resp)
                });
                PluginEvent::Attached {
                    plugin,
                    project: project_name,
                    tab,
                    result,
                }
            }
            Job::Shell { project } => {
                let project_name = project.name.clone();
                let result = self.connect(true).and_then(|conn| {
                    if !conn.caps.shell {
                        return Ok(None);
                    }
                    let request = Request::Attach(AttachRequest {
                        project,
                        session_id: None,
                        kind: AttachKind::Shell,
                        fresh: false,
                        name: "shell".into(),
                        wrk_sock: None,
                        wrk_tab: None,
                    });
                    let resp: AttachResponse = client::call(&conn.socket, &request, conn.timeout)?;
                    Ok((!resp.is_declined()).then_some(resp))
                });
                PluginEvent::Shell {
                    plugin,
                    project: project_name,
                    result,
                }
            }
            Job::List { project } => {
                let project_name = project.name.clone();
                let result = self.connect(true).and_then(|conn| {
                    if !conn.caps.list {
                        return Ok(Vec::new());
                    }
                    let request = Request::ListSessions { project };
                    let list: ListResponse = client::call(&conn.socket, &request, conn.timeout)?;
                    Ok(list.sessions)
                });
                PluginEvent::Sessions {
                    plugin,
                    project: project_name,
                    result,
                }
            }
            Job::Close {
                project,
                session_id,
            } => {
                if let Ok(conn) = self.connect(false)
                    && conn.caps.close
                {
                    let request = Request::Close {
                        project,
                        session_id,
                    };
                    let _ = client::call::<IgnoredAny>(&conn.socket, &request, conn.timeout);
                }
                return None;
            }
        })
    }

    /// Reach the plugin, launching it first when `launch` and nothing is
    /// listening, and learn its capabilities on first contact with a daemon.
    fn connect(&self, launch: bool) -> Result<Connection> {
        let config = self.config.as_ref().ok_or_else(|| {
            anyhow!(
                "there is no [plugin.{}] table in settings.toml",
                self.plugin
            )
        })?;
        let timeout = Duration::from_secs(config.request_timeout_secs());
        let mut state = lock(&self.state);
        state.reap();
        if !client::is_live(&state.socket) {
            state.caps = None; // whatever answers next may be a different daemon
            if !launch {
                return Err(anyhow!("it is not running"));
            }
            if let Some((at, message)) = &state.failure
                && at.elapsed() < RETRY_BACKOFF
            {
                return Err(anyhow!("{message}"));
            }
            let Some(command) = config.command.as_deref().filter(|c| !c.is_empty()) else {
                return Err(anyhow!(
                    "nothing is listening on {} and no `command` is configured to start it",
                    state.socket.display()
                ));
            };
            let startup = Duration::from_secs(config.startup_timeout_secs());
            match client::launch(command, &state.socket, &self.log, startup) {
                Ok(daemon) => {
                    state.daemon = Some(daemon);
                    state.failure = None;
                }
                Err(e) => {
                    let message = format!("starting the plugin failed: {e:#}");
                    state.failure = Some((Instant::now(), message.clone()));
                    return Err(anyhow!(message));
                }
            }
        }
        let caps = match state.caps {
            Some(caps) => caps,
            None => {
                // Best effort: a plugin that doesn't answer `hello` just has no
                // optional capabilities.
                let caps = client::call::<HelloResponse>(&state.socket, &Request::Hello, timeout)
                    .map(|hello| hello.capabilities)
                    .unwrap_or_default();
                state.caps = Some(caps);
                caps
            }
        };
        Ok(Connection {
            socket: state.socket.clone(),
            caps,
            timeout,
        })
    }
}

fn lock(state: &Mutex<PluginState>) -> MutexGuard<'_, PluginState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use wrk_plugin::proto;

    fn config(command: Option<Vec<String>>, socket: Option<&Path>) -> PluginConfig {
        PluginConfig {
            socket: socket.map(|s| s.display().to_string()),
            command,
            startup_timeout: Some(10),
            request_timeout: Some(5),
        }
    }

    fn host(dir: &Path, config: PluginConfig) -> PluginHost {
        let mut host = PluginHost::new(
            BTreeMap::from([("p".to_string(), config)]),
            dir.to_path_buf(),
        );
        host.set_wrk_sock(Some(&dir.join("wrk.sock")));
        host
    }

    fn project() -> ProjectPayload {
        ProjectPayload {
            name: "svc".into(),
            path: "/home/me/svc".into(),
            config: BTreeMap::new(),
        }
    }

    fn attach_job() -> Job {
        Job::Attach {
            project: project(),
            tab: "tab0".into(),
            session_id: None,
            fresh: true,
            name: "main".into(),
        }
    }

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    /// A minimal plugin: answers `hello` with `caps`, and `attach` with a
    /// command plus a session id naming the serving process (to tell a
    /// relaunched daemon from the original). Each attach request is sent to
    /// `seen`, if given.
    fn serve_fake(
        listener: UnixListener,
        caps: Capabilities,
        seen: Option<std::sync::mpsc::Sender<AttachRequest>>,
    ) {
        for stream in listener.incoming().flatten() {
            let mut line = String::new();
            if BufReader::new(&stream).read_line(&mut line).unwrap_or(0) == 0 {
                continue; // liveness probe
            }
            let reply = match serde_json::from_str::<Request>(&line).unwrap() {
                Request::Hello => proto::ok_reply(&HelloResponse {
                    name: "fake".into(),
                    version: "0".into(),
                    capabilities: caps,
                }),
                Request::Attach(request) => {
                    if let Some(seen) = &seen {
                        let _ = seen.send(request);
                    }
                    proto::ok_reply(&AttachResponse {
                        command: vec!["fake-agent".into()],
                        session_id: Some(format!("pid-{}", std::process::id())),
                        ..Default::default()
                    })
                }
                Request::ListSessions { .. } => proto::ok_reply(&ListResponse {
                    sessions: vec![PluginSession {
                        id: "s1".into(),
                        name: Some("one".into()),
                        state: Default::default(),
                    }],
                }),
                Request::Close { .. } => proto::ok_reply(&serde_json::json!({})),
            };
            let _ = (&stream).write_all(format!("{}\n", reply.unwrap()).as_bytes());
        }
    }

    /// An in-process fake plugin on `socket` (no launch needed).
    fn spawn_fake(socket: &Path, caps: Capabilities) -> std::sync::mpsc::Receiver<AttachRequest> {
        let listener = UnixListener::bind(socket).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || serve_fake(listener, caps, Some(tx)));
        rx
    }

    /// Not a real test: the body of a fake plugin *daemon*. Other tests launch
    /// this very test binary running only this test ([`fake_daemon_command`]),
    /// so the real launch path is exercised without depending on any other
    /// program being installed.
    #[test]
    #[ignore = "the body of a fake plugin daemon, launched by other tests"]
    fn fake_plugin_daemon() {
        let Some(socket) = std::env::var_os(client::SOCKET_ENV) else {
            return;
        };
        let all = Capabilities {
            list: true,
            shell: true,
            status: true,
            close: true,
        };
        serve_fake(UnixListener::bind(socket).unwrap(), all, None);
    }

    fn fake_daemon_command() -> Vec<String> {
        let exe = std::env::current_exe().unwrap();
        vec![
            exe.display().to_string(),
            "--exact".into(),
            "plugin::tests::fake_plugin_daemon".into(),
            "--ignored".into(),
            "--nocapture".into(),
        ]
    }

    /// Kills any daemon the host launched when the test ends, even on failure.
    struct StopDaemons(Vec<Arc<Mutex<PluginState>>>);

    impl StopDaemons {
        fn of(host: &PluginHost) -> Self {
            Self(host.states.values().cloned().collect())
        }
    }

    impl Drop for StopDaemons {
        fn drop(&mut self) {
            for state in &self.0 {
                if let Some(mut daemon) = lock(state).daemon.take() {
                    let _ = daemon.kill();
                    let _ = daemon.wait();
                }
            }
        }
    }

    fn attached(event: Option<PluginEvent>) -> Result<AttachResponse> {
        match event {
            Some(PluginEvent::Attached { result, .. }) => result,
            other => panic!("expected an attach result, got {other:?}"),
        }
    }

    #[test]
    fn launches_a_missing_plugin_and_relaunches_it_after_it_dies() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = host(dir.path(), config(Some(fake_daemon_command()), None));
        let first = attached(host.worker("p").run(attach_job())).expect("launch + attach");
        let _stop = StopDaemons::of(&host);
        assert_eq!(first.command, ["fake-agent"]);

        // A second job reuses the running daemon rather than launching another.
        let again = attached(host.worker("p").run(attach_job())).unwrap();
        assert_eq!(again.session_id, first.session_id);

        // Kill it behind wrk's back; the next job relaunches it.
        {
            let mut state = lock(&host.states["p"]);
            let daemon = state.daemon.as_mut().expect("wrk launched it");
            daemon.kill().unwrap();
            daemon.wait().unwrap();
        }
        let healed = attached(host.worker("p").run(attach_job())).expect("relaunched");
        assert_ne!(healed.session_id, first.session_id, "a new daemon answered");
    }

    /// The first live test's failure: the launch command dies at once. Every
    /// job of the burst a project open queues must fail fast with the reason,
    /// without relaunching once per job.
    #[test]
    fn a_broken_launch_fails_fast_once_for_a_burst_of_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("launches");
        let script = format!(
            "echo x >> '{}'; echo \"env: 'python3': No such file or directory\" >&2; exit 127",
            count.display()
        );
        let mut host = host(dir.path(), config(Some(sh(&script)), None));
        let started = Instant::now();
        for _ in 0..3 {
            host.submit("p", attach_job());
        }
        let mut errors = Vec::new();
        while errors.len() < 3 {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "jobs did not fail fast"
            );
            match host.try_recv() {
                Some(event) => errors.push(attached(Some(event)).unwrap_err().to_string()),
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        for error in &errors {
            assert!(error.contains("127"), "{error}");
            assert!(
                error.contains("'python3': No such file or directory"),
                "{error}"
            );
        }
        let launches = std::fs::read_to_string(&count).unwrap();
        assert_eq!(launches.lines().count(), 1, "relaunched within the backoff");
    }

    #[test]
    fn status_coordinates_are_offered_only_to_plugins_that_push_status() {
        for status in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("fake.sock");
            let seen = spawn_fake(
                &socket,
                Capabilities {
                    status,
                    ..Default::default()
                },
            );
            let mut host = host(dir.path(), config(None, Some(&socket)));
            attached(host.worker("p").run(attach_job())).unwrap();
            let request = seen.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(request.wrk_tab.as_deref(), Some("tab0"));
            assert_eq!(request.wrk_sock.is_some(), status, "status cap = {status}");
        }
    }

    #[test]
    fn optional_methods_respect_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fake.sock");
        let _seen = spawn_fake(&socket, Capabilities::default());
        let mut host = host(dir.path(), config(None, Some(&socket)));
        // No `shell` cap ⇒ a local shell; no `list` cap ⇒ no sessions to offer.
        match host.worker("p").run(Job::Shell { project: project() }) {
            Some(PluginEvent::Shell { result, .. }) => assert!(result.unwrap().is_none()),
            other => panic!("{other:?}"),
        }
        match host.worker("p").run(Job::List { project: project() }) {
            Some(PluginEvent::Sessions { result, .. }) => assert!(result.unwrap().is_empty()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn listing_returns_the_plugins_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fake.sock");
        let _seen = spawn_fake(
            &socket,
            Capabilities {
                list: true,
                ..Default::default()
            },
        );
        let mut host = host(dir.path(), config(None, Some(&socket)));
        match host.worker("p").run(Job::List { project: project() }) {
            Some(PluginEvent::Sessions { result, .. }) => {
                assert_eq!(result.unwrap()[0].id, "s1");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_close_notice_never_launches_the_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("launched");
        let script = format!("touch '{}'", marker.display());
        let mut host = host(dir.path(), config(Some(sh(&script)), None));
        let job = Job::Close {
            project: project(),
            session_id: "s1".into(),
        };
        assert!(host.worker("p").run(job).is_none());
        assert!(!marker.exists());
    }

    #[test]
    fn an_unconfigured_plugin_is_reported_not_guessed() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = PluginHost::new(BTreeMap::new(), dir.path().to_path_buf());
        let err = attached(host.worker("ghost").run(attach_job())).unwrap_err();
        assert!(err.to_string().contains("[plugin.ghost]"), "{err}");
    }
}
