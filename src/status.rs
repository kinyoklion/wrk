//! Claude Code status signalling.
//!
//! When wrk spawns a Claude session it sets `WRK_SOCK` (this instance's IPC
//! socket) and `WRK_TAB` (an opaque per-tab id). Claude Code hooks installed in
//! `~/.claude/settings.json` invoke `wrk hook <kind>`, which connects to
//! `WRK_SOCK` and pushes a one-line status update tagged with `WRK_TAB`. The
//! running TUI drains those updates and reflects them in the sidebar — no files,
//! no polling. Sessions not launched by wrk have no `WRK_SOCK`, so the guarded
//! hook command is a no-op for them.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// A status transition pushed by a Claude Code hook (`wrk hook <kind>`). This is
/// the wire vocabulary carried by `ipc::StatusUpdate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusKind {
    /// The user submitted a prompt — Claude is actively working.
    Busy,
    /// Claude finished its turn and is idle.
    Stopped,
    /// Claude is blocked waiting for the user (permission / input).
    Waiting,
    /// A sub-agent (Task tool) started.
    SubagentStart,
    /// A sub-agent finished.
    SubagentStop,
}

impl StatusKind {
    /// Parse the positional argument of `wrk hook <kind>`.
    pub fn from_arg(s: &str) -> Option<Self> {
        Some(match s {
            "busy" => Self::Busy,
            "stopped" => Self::Stopped,
            "waiting" => Self::Waiting,
            "subagent-start" => Self::SubagentStart,
            "subagent-stop" => Self::SubagentStop,
            _ => return None,
        })
    }
}

/// The last state-changing event seen for a tab (sub-agent start/stop only move
/// the counter, they don't change this).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    /// Actively working.
    Busy,
    /// Finished its turn, idle.
    Stopped,
    /// Waiting for the user (permission / input).
    Waiting,
}

/// Live status for a single Claude tab, updated from hook pushes. `event` is
/// `None` until the first hook fires, at which point the UI switches from the
/// idle-time heuristic to precise hook state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TabStatus {
    pub event: Option<HookEvent>,
    /// Number of sub-agents currently running (for future UI enrichment).
    pub subagents: u32,
}

impl TabStatus {
    /// Fold a hook update into this status.
    pub fn apply(&mut self, kind: StatusKind) {
        match kind {
            StatusKind::Busy => self.event = Some(HookEvent::Busy),
            StatusKind::Stopped => self.event = Some(HookEvent::Stopped),
            StatusKind::Waiting => self.event = Some(HookEvent::Waiting),
            StatusKind::SubagentStart => self.subagents = self.subagents.saturating_add(1),
            StatusKind::SubagentStop => self.subagents = self.subagents.saturating_sub(1),
        }
    }
}

/// Whether process `pid` is currently alive (Linux `/proc` check). Used to
/// reclaim runtime artifacts (sockets, review mirrors) left by dead instances.
/// A reused pid keeps its artifact around one extra generation — harmless, since
/// the live instance uses a different pid.
pub fn pid_is_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Base runtime directory for this user's wrk instance (holds the `sock/`
/// subdir). Prefers `$XDG_RUNTIME_DIR/wrk`, falling back to `/tmp/wrk-<user>`.
pub fn runtime_dir() -> PathBuf {
    if let Some(dirs) = directories::ProjectDirs::from("", "", "wrk")
        && let Some(runtime) = dirs.runtime_dir()
    {
        return runtime.to_path_buf();
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
    PathBuf::from(format!("/tmp/wrk-{user}"))
}

/// One installed hook: the Claude Code event, its tool matcher (empty = all),
/// and the `wrk hook` kind argument it fires.
struct HookSpec {
    event: &'static str,
    matcher: &'static str,
    kind: &'static str,
}

const HOOKS: &[HookSpec] = &[
    HookSpec {
        event: "UserPromptSubmit",
        matcher: "",
        kind: "busy",
    },
    HookSpec {
        event: "Stop",
        matcher: "",
        kind: "stopped",
    },
    HookSpec {
        event: "Notification",
        matcher: "",
        kind: "waiting",
    },
    HookSpec {
        event: "PreToolUse",
        matcher: "Task",
        kind: "subagent-start",
    },
    HookSpec {
        event: "SubagentStop",
        matcher: "",
        kind: "subagent-stop",
    },
];

/// Substrings that identify a hook entry as one wrk wrote. `WRK_SOCK` marks the
/// current socket-push commands; `WRK_STATUS_FILE` marks the legacy file-polling
/// commands so re-running `install-hooks` upgrades (and `uninstall-hooks`
/// removes) older installs.
const HOOK_MARKERS: &[&str] = &["WRK_SOCK", "WRK_STATUS_FILE"];

fn command_is_ours(cmd: &str) -> bool {
    HOOK_MARKERS.iter().any(|m| cmd.contains(m))
}

fn hook_command(kind: &str) -> String {
    // `$WRK_BIN` is the path to the running wrk, exported into every pane by the
    // instance itself — so nothing machine-specific is baked into settings.json
    // (portable across machines, and works under `cargo run`). Guard on WRK_SOCK
    // (set together with WRK_BIN only in wrk-spawned panes) so a Claude session
    // not launched by wrk is a no-op; the trailing `; true` keeps the hook's exit
    // status 0 either way. `wrk hook` itself never fails; output is swallowed.
    format!(r#"[ -n "$WRK_SOCK" ] && "$WRK_BIN" hook {kind} >/dev/null 2>&1; true"#)
}

fn settings_path() -> Result<PathBuf> {
    Ok(claude_dir()?.join("settings.json"))
}

fn claude_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME env var not set")?;
    Ok(PathBuf::from(home).join(".claude"))
}

/// Marker embedded in the generated SKILL.md so `uninstall_skill` only removes a
/// skill wrk itself wrote (never a user's own skill of the same name).
const SKILL_INSTALL_MARKER: &str = "installed by `wrk install-hooks`";

/// Directory (= `/command`) names of the skills wrk installs.
const SKILL_NAMES: &[&str] = &["wrk-view", "start-local-review", "end-local-review"];

/// The skills wrk installs into `~/.claude/skills/<name>/SKILL.md`. Every command
/// invokes `"$WRK_BIN"` — the path to the running instance, exported into each
/// pane — so nothing machine-specific is written to `~/.claude` (portable across
/// machines and correct under `cargo run`). The `!`…`` preprocessor and hook
/// shells inherit the pane env, so `$WRK_BIN` resolves there even without PATH.
fn skill_specs() -> Vec<(&'static str, String)> {
    vec![
        ("wrk-view", view_skill_markdown()),
        ("start-local-review", review_start_skill_markdown()),
        ("end-local-review", review_end_skill_markdown()),
    ]
}

fn marker_comment() -> String {
    format!("<!-- {SKILL_INSTALL_MARKER}; safe to delete, or run `wrk uninstall-hooks` -->")
}

/// The `wrk-view` skill: teaches Claude to open files with `wrk view`.
fn view_skill_markdown() -> String {
    format!(
        "---\n\
name: wrk-view\n\
description: Open a markdown file, README, or diagram in the wrk viewer. Use when the user asks to view, open, preview, show, or visualize a markdown/text file or diagram in the terminal.\n\
allowed-tools: Bash(\"$WRK_BIN\" view *)\n\
---\n\
{marker}\n\
\n\
# View a file in the wrk viewer\n\
\n\
Run `\"$WRK_BIN\" view <absolute-path>` to open a file in wrk's markdown viewer.\n\
Inside a wrk session it opens as a new tab beside the conversation; in a plain\n\
shell it opens a scrollable pager. It is read-only and never modifies the file.\n\
\n\
When the user asks to view, open, preview, show, or visualize a markdown file or\n\
diagram:\n\
\n\
1. Resolve it to an absolute path (search with grep/find if they described the\n\
   file rather than naming it).\n\
2. Run `\"$WRK_BIN\" view <absolute-path>`.\n\
3. Briefly confirm it is open.\n",
        marker = marker_comment()
    )
}

/// The `/start-local-review` skill: model-invocable, so Claude inspects the repo
/// and picks the comparison itself before opening the review overlay.
fn review_start_skill_markdown() -> String {
    format!(
        "---\n\
name: start-local-review\n\
description: Start an in-editor code review in wrk. Use when the user asks to review changes, review a diff, do a local/code review, or look over their work side-by-side.\n\
allowed-tools: Bash(\"$WRK_BIN\" review:*), Bash(git status:*), Bash(git log:*), Bash(git diff:*), Bash(git branch:*), Bash(git rev-parse:*)\n\
---\n\
{marker}\n\
\n\
# Start a local code review in wrk\n\
\n\
Open a side-by-side review in wrk so the user can comment on the diff. First\n\
work out WHAT to review, then start it — don't invent feedback yourself.\n\
\n\
1. Inspect the repository:\n\
   - `git status --porcelain` — are there uncommitted changes?\n\
   - `git branch --show-current`, and the base branch (`git rev-parse --abbrev-ref origin/HEAD` when it exists, else assume `main`/`master`).\n\
   - `git log --oneline <base>..HEAD` — are there local commits not on the base?\n\
2. Choose the target:\n\
   - If the user named one, use it.\n\
   - Else if there are uncommitted changes, review those: `\"$WRK_BIN\" review start` (no argument = working tree vs HEAD).\n\
   - Else if the branch has commits ahead of the base, review those: `\"$WRK_BIN\" review start <base>..HEAD`.\n\
3. Run `\"$WRK_BIN\" review start <target>`. Then tell the user to comment in the\n\
   wrk review pane and run `/end-local-review` when done. Wait for their\n\
   comments — do not guess at review feedback.\n",
        marker = marker_comment()
    )
}

/// The `/end-local-review` skill: pulls the user's comments via `wrk review end`
/// and hands them to Claude to act on. The `!`…`` preprocessor shell inherits the
/// pane env, so `$WRK_BIN` resolves without relying on PATH.
fn review_end_skill_markdown() -> String {
    format!(
        "---\n\
name: end-local-review\n\
description: End the in-editor code review in wrk and collect the user's comments. Use when the user says they are done reviewing, finished commenting, or asks to end the local review.\n\
allowed-tools: Bash(\"$WRK_BIN\" review:*)\n\
---\n\
{marker}\n\
\n\
# Collect local review comments\n\
\n\
!`\"$WRK_BIN\" review end`\n\
\n\
The output above lists the comments the user left in the wrk review pane (file,\n\
line, side, the comment, and the quoted line). Address each one:\n\
\n\
- Make the requested change, or\n\
- If you disagree or need clarification, say so and ask.\n\
\n\
Work through them in file order and finish with a short summary of what you\n\
changed.\n",
        marker = marker_comment()
    )
}

/// Write every wrk skill to `~/.claude/skills/<name>/SKILL.md`, overwriting any
/// prior copy (so content fixes propagate). Returns the written paths.
pub fn install_skills() -> Result<Vec<PathBuf>> {
    install_skills_in(&claude_dir()?)
}

/// Remove the wrk-installed skills. Returns the removed directories (only those
/// wrk wrote, identified by the install marker).
pub fn uninstall_skills() -> Result<Vec<PathBuf>> {
    uninstall_skills_in(&claude_dir()?)
}

fn install_skills_in(claude_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for (name, markdown) in skill_specs() {
        let dir = claude_dir.join("skills").join(name);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("SKILL.md");
        fs::write(&path, markdown).with_context(|| format!("writing {}", path.display()))?;
        written.push(path);
    }
    Ok(written)
}

fn uninstall_skills_in(claude_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    for name in SKILL_NAMES {
        let dir = claude_dir.join("skills").join(name);
        let path = dir.join("SKILL.md");
        if !path.exists() {
            continue;
        }
        // Only remove a skill we wrote — identified by our marker.
        let content = fs::read_to_string(&path).unwrap_or_default();
        if !content.contains(SKILL_INSTALL_MARKER) {
            continue;
        }
        fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
        removed.push(dir);
    }
    Ok(removed)
}

fn read_settings(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    let value: Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(value)
}

fn write_settings(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(value).context("serializing settings.json")?;
    fs::write(path, text + "\n").with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn install_hooks() -> Result<PathBuf> {
    let path = settings_path()?;
    install_hooks_at(&path)?;
    Ok(path)
}

/// Merge wrk's hook entries into the settings.json at `path`. Commands reference
/// `$WRK_BIN` (exported by the running instance) rather than a hardcoded path.
/// Split out from [`install_hooks`] so tests can drive it against a temp file.
fn install_hooks_at(path: &Path) -> Result<()> {
    let mut settings = read_settings(path)?;

    if !settings.is_object() {
        return Err(anyhow!("{} top-level is not a JSON object", path.display()));
    }
    let root = settings.as_object_mut().unwrap();
    let hooks_entry = root.entry("hooks".to_string()).or_insert_with(|| json!({}));
    if !hooks_entry.is_object() {
        return Err(anyhow!("settings.json: 'hooks' is not an object"));
    }
    let hooks_obj = hooks_entry.as_object_mut().unwrap();

    for spec in HOOKS {
        let arr_entry = hooks_obj
            .entry(spec.event.to_string())
            .or_insert_with(|| json!([]));
        if !arr_entry.is_array() {
            return Err(anyhow!(
                "settings.json: hooks.{} is not an array",
                spec.event
            ));
        }
        let arr = arr_entry.as_array_mut().unwrap();
        let new_cmd = hook_command(spec.kind);

        // If a wrk-marked entry exists, refresh its command (so re-running
        // install-hooks upgrades legacy file-polling commands and picks up bug
        // fixes). Otherwise append a new entry.
        let mut found = false;
        for entry in arr.iter_mut() {
            if !entry_has_marker(entry) {
                continue;
            }
            found = true;
            if let Some(hooks) = entry.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                for h in hooks.iter_mut() {
                    let is_ours = h
                        .get("command")
                        .and_then(|c| c.as_str())
                        .is_some_and(command_is_ours);
                    if is_ours {
                        h["command"] = json!(new_cmd.clone());
                    }
                }
            }
        }
        if !found {
            arr.push(json!({
                "matcher": spec.matcher,
                "hooks": [{
                    "type": "command",
                    "command": new_cmd,
                }],
            }));
        }
    }

    write_settings(path, &settings)
}

pub fn uninstall_hooks() -> Result<(PathBuf, usize)> {
    let path = settings_path()?;
    let removed = uninstall_hooks_at(&path)?;
    Ok((path, removed))
}

fn uninstall_hooks_at(path: &Path) -> Result<usize> {
    if !path.exists() {
        return Ok(0);
    }
    let mut settings = read_settings(path)?;
    let Some(root) = settings.as_object_mut() else {
        return Ok(0);
    };
    let Some(hooks_entry) = root.get_mut("hooks") else {
        return Ok(0);
    };
    let Some(hooks_obj) = hooks_entry.as_object_mut() else {
        return Ok(0);
    };

    let mut removed = 0usize;
    // Scan every hook event, not just the ones we install today — a legacy
    // install may have entries under events we no longer use.
    let event_keys: Vec<String> = hooks_obj.keys().cloned().collect();
    for event in &event_keys {
        if let Some(arr_entry) = hooks_obj.get_mut(event)
            && let Some(arr) = arr_entry.as_array_mut()
        {
            let before = arr.len();
            arr.retain(|entry| !entry_has_marker(entry));
            removed += before - arr.len();
        }
    }
    // Drop event arrays we emptied.
    for event in &event_keys {
        let drop = hooks_obj
            .get(event)
            .and_then(|v| v.as_array())
            .is_some_and(|a| a.is_empty());
        if drop {
            hooks_obj.remove(event);
        }
    }
    if hooks_obj.is_empty() {
        root.remove("hooks");
    }

    write_settings(path, &settings)?;
    Ok(removed)
}

fn entry_has_marker(entry: &Value) -> bool {
    let Some(hooks) = entry.get("hooks").and_then(|h| h.as_array()) else {
        return false;
    };
    hooks.iter().any(|h| {
        h.get("command")
            .and_then(|c| c.as_str())
            .is_some_and(command_is_ours)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_kind_round_trips_from_arg() {
        for (arg, kind) in [
            ("busy", StatusKind::Busy),
            ("stopped", StatusKind::Stopped),
            ("waiting", StatusKind::Waiting),
            ("subagent-start", StatusKind::SubagentStart),
            ("subagent-stop", StatusKind::SubagentStop),
        ] {
            assert_eq!(StatusKind::from_arg(arg), Some(kind));
        }
        assert_eq!(StatusKind::from_arg("nope"), None);
    }

    #[test]
    fn status_kind_serde_is_kebab_case() {
        assert_eq!(
            serde_json::to_string(&StatusKind::SubagentStart).unwrap(),
            "\"subagent-start\""
        );
        let back: StatusKind = serde_json::from_str("\"waiting\"").unwrap();
        assert_eq!(back, StatusKind::Waiting);
    }

    #[test]
    fn tab_status_apply_tracks_event_and_subagents() {
        let mut s = TabStatus::default();
        assert_eq!(s.event, None);
        s.apply(StatusKind::Busy);
        assert_eq!(s.event, Some(HookEvent::Busy));
        s.apply(StatusKind::SubagentStart);
        s.apply(StatusKind::SubagentStart);
        assert_eq!(s.subagents, 2);
        assert_eq!(s.event, Some(HookEvent::Busy)); // sub-agents don't change event
        s.apply(StatusKind::SubagentStop);
        assert_eq!(s.subagents, 1);
        s.apply(StatusKind::Stopped);
        assert_eq!(s.event, Some(HookEvent::Stopped));
        // Underflow is clamped, never panics.
        s.apply(StatusKind::SubagentStop);
        s.apply(StatusKind::SubagentStop);
        assert_eq!(s.subagents, 0);
    }

    #[test]
    fn hook_command_guards_on_socket_and_invokes_wrk_bin() {
        let cmd = hook_command("busy");
        assert!(cmd.contains(r#"[ -n "$WRK_SOCK" ]"#));
        // Invokes the instance-provided binary path, never a hardcoded one.
        assert!(cmd.contains(r#""$WRK_BIN" hook busy"#));
        assert!(cmd.trim_end().ends_with("; true"));
        assert!(command_is_ours(&cmd));
    }

    #[test]
    fn command_is_ours_matches_current_and_legacy() {
        assert!(command_is_ours(
            r#"[ -n "$WRK_SOCK" ] && wrk hook stopped; true"#
        ));
        assert!(command_is_ours(
            r#"printf 'Stop' > "$WRK_STATUS_FILE"; true"#
        ));
        assert!(!command_is_ours("notify-send done"));
    }

    #[test]
    fn install_writes_all_events_with_matchers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        install_hooks_at(&path).unwrap();

        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let hooks = v["hooks"].as_object().unwrap();
        for spec in HOOKS {
            let arr = hooks[spec.event].as_array().unwrap();
            assert_eq!(arr.len(), 1, "event {}", spec.event);
            assert_eq!(arr[0]["matcher"], spec.matcher);
            let cmd = arr[0]["hooks"][0]["command"].as_str().unwrap();
            assert!(
                cmd.contains(&format!("hook {}", spec.kind)),
                "event {}",
                spec.event
            );
        }
    }

    #[test]
    fn install_upgrades_legacy_entries_without_duplicating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        // A legacy file-polling install.
        let legacy = json!({
            "hooks": {
                "Stop": [{
                    "matcher": "",
                    "hooks": [{ "type": "command", "command": "printf 'Stop' > \"$WRK_STATUS_FILE\"; true" }]
                }]
            }
        });
        fs::write(&path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

        install_hooks_at(&path).unwrap();
        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        // Refreshed in place — not duplicated.
        assert_eq!(stop.len(), 1);
        let cmd = stop[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains(r#""$WRK_BIN" hook stopped"#));
        assert!(!cmd.contains("WRK_STATUS_FILE"));
    }

    #[test]
    fn uninstall_removes_current_and_legacy_and_preserves_others() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let settings = json!({
            "hooks": {
                "Stop": [
                    { "matcher": "", "hooks": [{ "type": "command", "command": "printf 'Stop' > \"$WRK_STATUS_FILE\"; true" }] },
                    { "matcher": "", "hooks": [{ "type": "command", "command": "my-own-hook" }] }
                ],
                "PreToolUse": [
                    { "matcher": "Task", "hooks": [{ "type": "command", "command": "[ -n \"$WRK_SOCK\" ] && wrk hook subagent-start; true" }] }
                ]
            }
        });
        fs::write(&path, serde_json::to_string_pretty(&settings).unwrap()).unwrap();

        let removed = uninstall_hooks_at(&path).unwrap();
        assert_eq!(removed, 2);
        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        // The user's own Stop hook survives; the emptied PreToolUse array is gone.
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert_eq!(stop[0]["hooks"][0]["command"], "my-own-hook");
        assert!(v["hooks"].get("PreToolUse").is_none());
    }

    #[test]
    fn skills_use_wrk_bin_and_carry_markers() {
        // Every skill carries the marker and invokes `"$WRK_BIN"` (the instance-
        // provided path) — never a hardcoded path; the review-start skill stays
        // model-invocable; the end skill pulls comments via `wrk review end`.
        for (name, md) in skill_specs() {
            assert!(md.contains(&format!("name: {name}")), "{name} name");
            assert!(md.contains(SKILL_INSTALL_MARKER), "{name} marker");
            assert!(md.contains("\"$WRK_BIN\""), "{name} uses $WRK_BIN");
        }
        assert!(review_end_skill_markdown().contains("!`\"$WRK_BIN\" review end`"));
        let start = review_start_skill_markdown();
        assert!(start.contains("\"$WRK_BIN\" review start"));
        assert!(!start.contains("disable-model-invocation"));
    }

    /// Regression guard: nothing wrk writes into `~/.claude` (hook commands or
    /// skills) may embed an absolute filesystem path — those are machine-specific
    /// and break when the config is used on another machine or under `cargo run`.
    /// Everything must go through `$WRK_BIN`.
    #[test]
    fn no_machine_specific_paths_in_hooks_or_skills() {
        let looks_absolute = |s: &str| {
            s.split_whitespace()
                .any(|w| w.trim_matches('"').starts_with('/'))
        };
        for spec in HOOKS {
            let cmd = hook_command(spec.kind);
            assert!(
                cmd.contains("$WRK_BIN"),
                "hook {} must use $WRK_BIN",
                spec.kind
            );
            assert!(
                !looks_absolute(&cmd),
                "hook {} embeds an absolute path: {cmd}",
                spec.kind
            );
        }
        for (name, md) in skill_specs() {
            // Skill bodies legitimately mention `<absolute-path>` as a placeholder;
            // check only that no line invokes wrk via an absolute path.
            for line in md
                .lines()
                .filter(|l| l.contains("WRK") || l.contains("review") || l.contains("view"))
            {
                assert!(
                    !line.contains("/wrk ") && !line.contains("/wrk\""),
                    "{name} line embeds an absolute wrk path: {line}"
                );
            }
        }
    }

    #[test]
    fn install_then_uninstall_skills_round_trip() {
        let home = tempfile::tempdir().unwrap();
        let claude = home.path().join(".claude");

        let paths = install_skills_in(&claude).unwrap();
        assert_eq!(paths.len(), SKILL_NAMES.len());
        assert!(claude.join("skills/wrk-view/SKILL.md").exists());
        assert!(claude.join("skills/start-local-review/SKILL.md").exists());
        assert!(claude.join("skills/end-local-review/SKILL.md").exists());

        let removed = uninstall_skills_in(&claude).unwrap();
        assert_eq!(removed.len(), SKILL_NAMES.len());
        assert!(!claude.join("skills/wrk-view/SKILL.md").exists());

        // Second uninstall removes nothing.
        assert!(uninstall_skills_in(&claude).unwrap().is_empty());
    }

    #[test]
    fn uninstall_leaves_foreign_skill_untouched() {
        let home = tempfile::tempdir().unwrap();
        let claude = home.path().join(".claude");
        let dir = claude.join("skills/wrk-view");
        fs::create_dir_all(&dir).unwrap();
        // A user's own same-named skill (no wrk marker) must survive.
        fs::write(dir.join("SKILL.md"), "---\nname: wrk-view\n---\nmine\n").unwrap();

        let removed = uninstall_skills_in(&claude).unwrap();
        assert!(!removed.contains(&dir));
        assert!(dir.join("SKILL.md").exists());
    }
}
