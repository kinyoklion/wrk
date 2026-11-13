//! wrk's side of the protocol: probing, request/reply, and launching a plugin.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde::de::DeserializeOwned;

use crate::proto::{Request, parse_reply};

/// Environment variable naming the control socket a launched plugin must bind.
pub const SOCKET_ENV: &str = "WRK_PLUGIN_SOCK";

/// How often [`launch`] checks whether the daemon is up (or has died).
const LAUNCH_POLL: Duration = Duration::from_millis(20);

/// Longest log excerpt quoted in a launch error.
const TAIL_CHARS: usize = 300;

/// Is anything listening on `socket`? Connecting is the authoritative test: a
/// socket file left behind by a dead process refuses the connection. The probe
/// connects and immediately hangs up, so a plugin sees an empty request.
pub fn is_live(socket: &Path) -> bool {
    UnixStream::connect(socket).is_ok()
}

/// Send `request` and parse the reply as `T`. `timeout` bounds the write and
/// the wait for the reply, so a hung plugin costs at most that long.
pub fn call<T: DeserializeOwned>(socket: &Path, request: &Request, timeout: Duration) -> Result<T> {
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("connecting to {}", socket.display()))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|e| io_error(e, "sending the request", timeout))?;
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .map_err(|e| io_error(e, "waiting for a reply", timeout))?;
    parse_reply(&reply)
}

fn io_error(e: std::io::Error, doing: &str, timeout: Duration) -> anyhow::Error {
    match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => {
            anyhow!("no response after {}s {doing}", timeout.as_secs_f32())
        }
        _ => anyhow!("{doing}: {e}"),
    }
}

/// Start a plugin daemon and wait until it accepts connections on `socket`.
///
/// `command` runs with [`SOCKET_ENV`] set to `socket`, in its own process group
/// (so signals aimed at wrk's terminal, like a hangup, don't reach it), with
/// stdin closed and stdout/stderr appended to `log`. Startup fails fast:
///
/// - if the process exits before listening, the error carries its exit status
///   and the last line it logged (e.g. `env: 'python3': No such file or
///   directory`);
/// - if it neither listens nor exits within `startup_timeout`, it is killed.
///
/// A stale socket file (one nothing listens on) is removed first, so a plugin
/// that doesn't unlink before binding still starts. On success the [`Child`] is
/// returned for the caller to reap; dropping it does *not* stop the daemon, which
/// may intentionally outlive wrk.
pub fn launch(
    command: &[String],
    socket: &Path,
    log: &Path,
    startup_timeout: Duration,
) -> Result<Child> {
    let (program, args) = command
        .split_first()
        .filter(|(program, _)| !program.is_empty())
        .ok_or_else(|| anyhow!("the launch command is empty"))?;
    for dir in [socket.parent(), log.parent()].into_iter().flatten() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    remove_stale_socket(socket);

    let mut log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("opening {}", log.display()))?;
    writeln!(log_file, "--- wrk: launching {command:?} ---")?;
    let output_start = log_file.metadata().map(|m| m.len()).unwrap_or(0);

    let mut child = Command::new(program)
        .args(args)
        .env(SOCKET_ENV, socket)
        .stdin(Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file)
        .process_group(0)
        .spawn()
        .map_err(|e| anyhow!("could not run `{program}`: {e}"))?;

    let deadline = Instant::now() + startup_timeout;
    loop {
        if is_live(socket) {
            return Ok(child);
        }
        if let Some(status) = child.try_wait()? {
            return Err(anyhow!(
                "the process exited ({status}) before listening{}",
                log_excerpt(log, output_start)
            ));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!(
                "the process did not listen on {} within {}s and was stopped{}",
                socket.display(),
                startup_timeout.as_secs_f32(),
                log_excerpt(log, output_start)
            ));
        }
        std::thread::sleep(LAUNCH_POLL);
    }
}

/// Remove `socket` if it is a socket file nothing listens on. Anything else —
/// a live socket, a regular file at a misconfigured path — is left alone.
fn remove_stale_socket(socket: &Path) {
    if let Ok(meta) = fs::symlink_metadata(socket)
        && meta.file_type().is_socket()
        && !is_live(socket)
    {
        let _ = fs::remove_file(socket);
    }
}

/// `": <last line the plugin logged> (log: <path>)"`, or just the log pointer
/// when it printed nothing since `offset`.
fn log_excerpt(log: &Path, offset: u64) -> String {
    let last_line = (|| {
        let mut file = fs::File::open(log).ok()?;
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut output = String::new();
        file.read_to_string(&mut output).ok()?;
        let line = output
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| !l.is_empty())?;
        Some(line.chars().take(TAIL_CHARS).collect::<String>())
    })();
    match last_line {
        Some(line) => format!(": {line} (log: {})", log.display()),
        None => format!(" (log: {})", log.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{HelloResponse, error_reply, ok_reply};
    use std::os::unix::net::UnixListener;

    /// Serve `replies` (one per connection) on `socket` from a thread.
    fn serve(socket: &Path, replies: Vec<String>) {
        let listener = UnixListener::bind(socket).unwrap();
        std::thread::spawn(move || {
            for reply in replies {
                let (stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                (&stream)
                    .write_all(format!("{reply}\n").as_bytes())
                    .unwrap();
            }
        });
    }

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    #[test]
    fn call_parses_a_reply_and_surfaces_plugin_errors() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("p.sock");
        let hello = HelloResponse {
            name: "demo".into(),
            ..Default::default()
        };
        serve(
            &socket,
            vec![ok_reply(&hello).unwrap(), error_reply("box is gone")],
        );
        let t = Duration::from_secs(5);
        let got: HelloResponse = call(&socket, &Request::Hello, t).unwrap();
        assert_eq!(got.name, "demo");
        let err = call::<HelloResponse>(&socket, &Request::Hello, t).unwrap_err();
        assert_eq!(err.to_string(), "box is gone");
    }

    #[test]
    fn call_times_out_on_a_silent_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("p.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        // Accept and hold the connection open without ever replying.
        let holder = std::thread::spawn(move || listener.accept().map(|(s, _)| s));
        let started = Instant::now();
        let err = call::<HelloResponse>(&socket, &Request::Hello, Duration::from_millis(200))
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(err.to_string().contains("no response"), "{err}");
        drop(holder);
    }

    /// The failure that broke the first live test: the launch command dies
    /// immediately. It must fail at once — not after the startup timeout — and
    /// say why.
    #[test]
    fn launch_fails_fast_with_the_exit_status_and_last_log_line() {
        let dir = tempfile::tempdir().unwrap();
        let (socket, log) = (dir.path().join("p.sock"), dir.path().join("p.log"));
        let started = Instant::now();
        let err = launch(
            &sh("echo \"env: 'python3': No such file or directory\" >&2; exit 127"),
            &socket,
            &log,
            Duration::from_secs(30),
        )
        .unwrap_err()
        .to_string();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
        assert!(err.contains("127"), "{err}");
        assert!(
            err.contains("'python3': No such file or directory"),
            "{err}"
        );
        assert!(err.contains(&log.display().to_string()), "{err}");
    }

    #[test]
    fn launch_stops_a_daemon_that_never_listens() {
        let dir = tempfile::tempdir().unwrap();
        let (socket, log) = (dir.path().join("p.sock"), dir.path().join("p.log"));
        let err = launch(
            &sh("echo warming up; sleep 30"),
            &socket,
            &log,
            Duration::from_millis(300),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("did not listen"), "{err}");
        assert!(err.contains("warming up"), "{err}");
    }

    #[test]
    fn launch_reports_a_missing_program() {
        let dir = tempfile::tempdir().unwrap();
        let err = launch(
            &["/nonexistent/wrk-plugin-binary".to_string()],
            &dir.path().join("p.sock"),
            &dir.path().join("p.log"),
            Duration::from_secs(5),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("could not run"), "{err}");
        assert!(
            launch(
                &[],
                &dir.path().join("p.sock"),
                &dir.path().join("p.log"),
                Duration::from_secs(1)
            )
            .is_err()
        );
    }

    #[test]
    fn only_dead_socket_files_are_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("stale.sock");
        drop(UnixListener::bind(&stale).unwrap()); // leaves the file, nothing listening
        let live = dir.path().join("live.sock");
        let _listener = UnixListener::bind(&live).unwrap();
        let regular = dir.path().join("not-a-socket");
        fs::write(&regular, "keep me").unwrap();

        for path in [&live, &regular] {
            remove_stale_socket(path);
        }
        // A child forked by a concurrently running test holds copies of every
        // open fd until its exec, which can briefly keep the dropped listener
        // connectable — so allow a moment for the stale socket to read as dead.
        let deadline = Instant::now() + Duration::from_secs(2);
        while stale.exists() && Instant::now() < deadline {
            remove_stale_socket(&stale);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!stale.exists());
        assert!(live.exists());
        assert_eq!(fs::read_to_string(&regular).unwrap(), "keep me");
    }
}
