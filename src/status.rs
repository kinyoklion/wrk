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
    write_skills_in(&claude_dir()?, &skill_specs())
}

/// Remove the wrk-installed skills. Returns the removed directories (only those
/// wrk wrote, identified by the install marker).
pub fn uninstall_skills() -> Result<Vec<PathBuf>> {
    uninstall_skills_in(&claude_dir()?)
}

/// Write each `(name, markdown)` skill under `<base_dir>/skills/<name>/SKILL.md`,
/// overwriting any prior copy. Shared by the Claude and Kimi installers — only
/// the base directory and the skill bodies differ.
fn write_skills_in(base_dir: &Path, specs: &[(&'static str, String)]) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for (name, markdown) in specs {
        let dir = base_dir.join("skills").join(name);
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

// -----------------------------------------------------------------------------
// Kimi Coder harness
//
// Kimi keeps hooks in a TOML `[[hooks]]` array in `~/.kimi-code/config.toml` (not
// JSON like Claude), and skills in `~/.kimi-code/skills/<name>/SKILL.md` with its
// own frontmatter and `/skill:` invocation. Because that config file also holds
// the user's hand-written, commented provider/model config, hooks are merged with
// `toml_edit` (format-preserving) so only our `[[hooks]]` entries change. The hook
// commands push status over the same socket as Claude's (`wrk hook <kind>`); the
// extra `--harness kimi` flag tells `wrk hook` to also read the session id Kimi
// passes on stdin, so a Kimi tab's session can be learned and persisted.
// -----------------------------------------------------------------------------

/// Kimi's config/data home: `$KIMI_CODE_HOME` when set, else `~/.kimi-code`
/// (mirrors Kimi Code's own resolution so hooks/skills land where it looks).
fn kimi_dir() -> Result<PathBuf> {
    if let Ok(home) = std::env::var("KIMI_CODE_HOME")
        && !home.is_empty()
    {
        return Ok(PathBuf::from(home));
    }
    let home = std::env::var("HOME").context("HOME env var not set")?;
    Ok(PathBuf::from(home).join(".kimi-code"))
}

fn kimi_config_path() -> Result<PathBuf> {
    Ok(kimi_dir()?.join("config.toml"))
}

/// One Kimi hook: the lifecycle event, an optional regex matcher (empty = all,
/// omitted from the TOML), and the `wrk hook` kind it fires.
struct KimiHookSpec {
    event: &'static str,
    matcher: &'static str,
    kind: &'static str,
}

/// Kimi lifecycle events mapped onto wrk's shared status vocabulary. `SessionStart`
/// (fires on both startup and resume) seeds the session id and marks the tab idle;
/// `PermissionResult` clears the "waiting" dot once the user approves.
const KIMI_HOOKS: &[KimiHookSpec] = &[
    KimiHookSpec {
        event: "SessionStart",
        matcher: "",
        kind: "stopped",
    },
    KimiHookSpec {
        event: "UserPromptSubmit",
        matcher: "",
        kind: "busy",
    },
    KimiHookSpec {
        event: "Stop",
        matcher: "",
        kind: "stopped",
    },
    KimiHookSpec {
        event: "PermissionRequest",
        matcher: "",
        kind: "waiting",
    },
    KimiHookSpec {
        event: "PermissionResult",
        matcher: "",
        kind: "busy",
    },
    KimiHookSpec {
        event: "SubagentStart",
        matcher: "",
        kind: "subagent-start",
    },
    KimiHookSpec {
        event: "SubagentStop",
        matcher: "",
        kind: "subagent-stop",
    },
];

/// The shell command for a Kimi hook. Same guard/`$WRK_BIN` shape as Claude's
/// (see [`hook_command`]) so `command_is_ours` recognizes it; the extra
/// `--harness kimi` makes `wrk hook` read the session id off stdin.
fn kimi_hook_command(kind: &str) -> String {
    format!(r#"[ -n "$WRK_SOCK" ] && "$WRK_BIN" hook {kind} --harness kimi >/dev/null 2>&1; true"#)
}

/// Whether a `[[hooks]]` table's `command` is one wrk wrote.
fn kimi_table_is_ours(table: &toml_edit::Table) -> bool {
    table
        .get("command")
        .and_then(|v| v.as_str())
        .is_some_and(command_is_ours)
}

pub fn install_kimi_hooks() -> Result<PathBuf> {
    let path = kimi_config_path()?;
    install_kimi_hooks_at(&path)?;
    Ok(path)
}

/// Merge wrk's `[[hooks]]` into the Kimi `config.toml` at `path`, preserving every
/// other table and all comments. Our previous entries (matched by command marker)
/// are dropped and rewritten so re-running refreshes commands without duplicating.
fn install_kimi_hooks_at(path: &Path) -> Result<()> {
    use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

    let text = if path.exists() {
        fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
    } else {
        String::new()
    };
    let mut doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;

    if doc.get("hooks").is_none() {
        doc["hooks"] = Item::ArrayOfTables(ArrayOfTables::new());
    }
    let hooks = doc["hooks"]
        .as_array_of_tables_mut()
        .ok_or_else(|| anyhow!("{}: `hooks` is not an array of tables", path.display()))?;

    // Drop our previous entries (highest index first so removals don't shift).
    let ours: Vec<usize> = (0..hooks.len())
        .filter(|&i| hooks.get(i).is_some_and(kimi_table_is_ours))
        .collect();
    for i in ours.into_iter().rev() {
        hooks.remove(i);
    }
    // Append a fresh entry per spec.
    for spec in KIMI_HOOKS {
        let mut t = Table::new();
        t["event"] = value(spec.event);
        if !spec.matcher.is_empty() {
            t["matcher"] = value(spec.matcher);
        }
        t["command"] = value(kimi_hook_command(spec.kind));
        hooks.push(t);
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(path, doc.to_string()).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn uninstall_kimi_hooks() -> Result<(PathBuf, usize)> {
    let path = kimi_config_path()?;
    let removed = uninstall_kimi_hooks_at(&path)?;
    Ok((path, removed))
}

/// Remove wrk's `[[hooks]]` entries from the Kimi `config.toml` at `path`, leaving
/// everything else untouched. Returns how many entries were removed.
fn uninstall_kimi_hooks_at(path: &Path) -> Result<usize> {
    use toml_edit::DocumentMut;

    if !path.exists() {
        return Ok(0);
    }
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;

    let Some(hooks) = doc
        .get_mut("hooks")
        .and_then(|i| i.as_array_of_tables_mut())
    else {
        return Ok(0);
    };
    let ours: Vec<usize> = (0..hooks.len())
        .filter(|&i| hooks.get(i).is_some_and(kimi_table_is_ours))
        .collect();
    let removed = ours.len();
    for i in ours.into_iter().rev() {
        hooks.remove(i);
    }
    let empty = hooks.is_empty();
    if empty {
        doc.remove("hooks");
    }
    fs::write(path, doc.to_string()).with_context(|| format!("writing {}", path.display()))?;
    Ok(removed)
}

/// The Kimi skills wrk installs into `~/.kimi-code/skills/<name>/SKILL.md`. Same
/// three commands as the Claude skills, but in Kimi's frontmatter (`name`,
/// `description`, `whenToUse`; no `allowed-tools`) and invoked as `/skill:<name>`.
/// Kimi has no `!`…`` output-injection preprocessor, so `end-local-review` tells
/// the model to run `"$WRK_BIN" review end` itself and act on the output.
fn kimi_skill_specs() -> Vec<(&'static str, String)> {
    vec![
        ("wrk-view", kimi_view_skill_markdown()),
        ("start-local-review", kimi_review_start_skill_markdown()),
        ("end-local-review", kimi_review_end_skill_markdown()),
    ]
}

fn kimi_view_skill_markdown() -> String {
    format!(
        "---\n\
name: wrk-view\n\
description: Open a markdown file, README, or diagram in the wrk viewer. Use when the user asks to view, open, preview, show, or visualize a markdown/text file or diagram in the terminal.\n\
whenToUse: When the user asks to view, open, preview, show, or visualize a markdown file or diagram in the terminal.\n\
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

fn kimi_review_start_skill_markdown() -> String {
    format!(
        "---\n\
name: start-local-review\n\
description: Start an in-editor code review in wrk. Use when the user asks to review changes, review a diff, do a local/code review, or look over their work side-by-side.\n\
whenToUse: When the user asks to review changes, review a diff, do a local/code review, or look over their work side-by-side.\n\
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
   wrk review pane and run `/skill:end-local-review` when done. Wait for their\n\
   comments — do not guess at review feedback.\n",
        marker = marker_comment()
    )
}

fn kimi_review_end_skill_markdown() -> String {
    format!(
        "---\n\
name: end-local-review\n\
description: End the in-editor code review in wrk and collect the user's comments. Use when the user says they are done reviewing, finished commenting, or asks to end the local review.\n\
whenToUse: When the user says they are done reviewing, finished commenting, or asks to end the local review.\n\
---\n\
{marker}\n\
\n\
# Collect local review comments\n\
\n\
Run this command and read its output:\n\
\n\
```\n\
\"$WRK_BIN\" review end\n\
```\n\
\n\
The output lists the comments the user left in the wrk review pane (file, line,\n\
side, the comment, and the quoted line). Address each one:\n\
\n\
- Make the requested change, or\n\
- If you disagree or need clarification, say so and ask.\n\
\n\
Work through them in file order and finish with a short summary of what you\n\
changed.\n",
        marker = marker_comment()
    )
}

/// Write the Kimi skills into `~/.kimi-code/skills/<name>/SKILL.md`.
pub fn install_kimi_skills() -> Result<Vec<PathBuf>> {
    write_skills_in(&kimi_dir()?, &kimi_skill_specs())
}

/// Remove the wrk-installed Kimi skills (only those carrying wrk's marker).
pub fn uninstall_kimi_skills() -> Result<Vec<PathBuf>> {
    uninstall_skills_in(&kimi_dir()?)
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

        let paths = write_skills_in(&claude, &skill_specs()).unwrap();
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

    // --- Kimi harness ---

    #[test]
    fn kimi_hook_command_shape_and_marker() {
        let cmd = kimi_hook_command("busy");
        assert!(cmd.contains(r#"[ -n "$WRK_SOCK" ]"#));
        assert!(cmd.contains(r#""$WRK_BIN" hook busy --harness kimi"#));
        assert!(cmd.trim_end().ends_with("; true"));
        // Recognized as ours by the shared marker check (so re-install refreshes
        // and uninstall removes it), and embeds no machine-specific path.
        assert!(command_is_ours(&cmd));
        assert!(
            !cmd.split_whitespace()
                .any(|w| w.trim_matches('"').starts_with('/'))
        );
    }

    #[test]
    fn install_kimi_hooks_preserves_other_tables_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // A user config with a comment, a scalar, and a provider table.
        fs::write(
            &path,
            "# my kimi config\ndefault_model = \"lmstudio/local\"\n\n\
             [providers.lmstudio]\ntype = \"openai\"\nbase_url = \"http://localhost:1234/v1\"\n",
        )
        .unwrap();

        install_kimi_hooks_at(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        // Untouched user content survives verbatim.
        assert!(text.contains("# my kimi config"));
        assert!(text.contains("default_model = \"lmstudio/local\""));
        assert!(text.contains("[providers.lmstudio]"));
        // Our hooks landed and parse as a valid array of tables.
        let doc: toml_edit::DocumentMut = text.parse().unwrap();
        let hooks = doc["hooks"].as_array_of_tables().unwrap();
        assert_eq!(hooks.len(), KIMI_HOOKS.len());
        // Re-running does not duplicate (matched by marker and rewritten).
        install_kimi_hooks_at(&path).unwrap();
        let doc: toml_edit::DocumentMut = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(
            doc["hooks"].as_array_of_tables().unwrap().len(),
            KIMI_HOOKS.len()
        );
    }

    #[test]
    fn uninstall_kimi_hooks_removes_only_ours() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // A user's own hook alongside a scalar; then install ours on top.
        fs::write(
            &path,
            "default_model = \"lmstudio/local\"\n\n\
             [[hooks]]\nevent = \"Stop\"\ncommand = \"notify-send done\"\n",
        )
        .unwrap();
        install_kimi_hooks_at(&path).unwrap();

        let removed = uninstall_kimi_hooks_at(&path).unwrap();
        assert_eq!(removed, KIMI_HOOKS.len());
        let text = fs::read_to_string(&path).unwrap();
        // The user's own hook and scalar survive; none of ours remain.
        assert!(text.contains("notify-send done"));
        assert!(text.contains("default_model = \"lmstudio/local\""));
        assert!(!text.contains("$WRK_BIN"));
        // A second uninstall removes nothing.
        assert_eq!(uninstall_kimi_hooks_at(&path).unwrap(), 0);
    }

    #[test]
    fn uninstall_kimi_hooks_drops_empty_array() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        install_kimi_hooks_at(&path).unwrap();
        uninstall_kimi_hooks_at(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        // No stray empty `[[hooks]]` left behind.
        assert!(!text.contains("[[hooks]]"));
    }

    #[test]
    fn kimi_skills_use_wrk_bin_carry_markers_and_slash_skill() {
        for (name, md) in kimi_skill_specs() {
            assert!(md.contains(&format!("name: {name}")), "{name} name");
            assert!(md.contains(SKILL_INSTALL_MARKER), "{name} marker");
            assert!(md.contains("\"$WRK_BIN\""), "{name} uses $WRK_BIN");
            // Kimi frontmatter, not Claude's.
            assert!(!md.contains("allowed-tools"), "{name} has no allowed-tools");
        }
        // The end-review skill has the model run the command (no Claude `!`…`` injection).
        let end = kimi_review_end_skill_markdown();
        assert!(end.contains("\"$WRK_BIN\" review end"));
        assert!(!end.contains("!`"));
        // The start-review skill points at the Kimi-style command name.
        assert!(kimi_review_start_skill_markdown().contains("/skill:end-local-review"));
    }

    #[test]
    fn kimi_skills_round_trip_and_preserve_foreign() {
        let home = tempfile::tempdir().unwrap();
        let kimi = home.path().join(".kimi-code");

        let paths = write_skills_in(&kimi, &kimi_skill_specs()).unwrap();
        assert_eq!(paths.len(), SKILL_NAMES.len());
        assert!(kimi.join("skills/wrk-view/SKILL.md").exists());

        // A user's own same-named skill (no marker) survives uninstall.
        let foreign = kimi.join("skills/mine");
        fs::create_dir_all(&foreign).unwrap();
        fs::write(foreign.join("SKILL.md"), "---\nname: mine\n---\nmine\n").unwrap();

        let removed = uninstall_skills_in(&kimi).unwrap();
        assert_eq!(removed.len(), SKILL_NAMES.len());
        assert!(!kimi.join("skills/wrk-view/SKILL.md").exists());
        assert!(foreign.join("SKILL.md").exists());
    }
}
