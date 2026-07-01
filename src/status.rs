//! Per-project status files written by Claude Code hooks.
//!
//! When wrk spawns a Claude session, it sets `WRK_STATUS_FILE` to a
//! per-project path under `$XDG_RUNTIME_DIR/wrk/status/` (or a `/tmp`
//! fallback). Hooks installed in `~/.claude/settings.json` write the
//! event name into that file each time Claude transitions state.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};

/// Hook events we care about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    UserPromptSubmit,
    Stop,
    Notification,
}

const EVENTS: &[(&str, &str)] = &[
    ("UserPromptSubmit", "UserPromptSubmit"),
    ("Stop", "Stop"),
    ("Notification", "Notification"),
];

/// Substring marker that identifies an entry as one of ours when scanning
/// existing settings.json (e.g. for uninstall or de-dup).
const HOOK_MARKER: &str = "WRK_STATUS_FILE";

pub fn status_dir() -> PathBuf {
    if let Some(dirs) = directories::ProjectDirs::from("", "", "wrk")
        && let Some(runtime) = dirs.runtime_dir()
    {
        return runtime.join("status");
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
    PathBuf::from(format!("/tmp/wrk-{user}/status"))
}

pub fn ensure_status_dir() -> Result<PathBuf> {
    let dir = status_dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// Status file path for an individual Claude tab, keyed by a runtime-assigned
/// `tab_status_id` (an opaque string unique per spawned tab).
pub fn status_file_for_tab(tab_status_id: &str) -> PathBuf {
    status_dir().join(format!("{}.status", sanitize(tab_status_id)))
}

/// Remove a tab's status file (best-effort). Called when a project's session
/// is unloaded so a stale status dot doesn't linger in the sidebar.
pub fn remove_tab_status(tab_status_id: &str) {
    let _ = fs::remove_file(status_file_for_tab(tab_status_id));
}

pub fn read_tab_status(tab_status_id: &str) -> Option<HookEvent> {
    let path = status_file_for_tab(tab_status_id);
    let content = fs::read_to_string(&path).ok()?;
    parse_event(content.trim())
}

fn parse_event(s: &str) -> Option<HookEvent> {
    match s {
        "UserPromptSubmit" => Some(HookEvent::UserPromptSubmit),
        "Stop" => Some(HookEvent::Stop),
        "Notification" => Some(HookEvent::Notification),
        _ => None,
    }
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn hook_command(event: &str) -> String {
    // The trailing `; true` ensures we exit 0 even when WRK_STATUS_FILE is
    // unset (i.e. the session wasn't launched by wrk) — otherwise the failed
    // `[ -n "" ]` test would propagate as exit 1 and Claude Code would log a
    // hook error.
    format!(r#"[ -n "$WRK_STATUS_FILE" ] && printf '{event}' > "$WRK_STATUS_FILE"; true"#)
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

/// The `wrk-view` skill markdown. Teaches Claude to open files with `wrk view`.
/// The frontmatter `description` doubles as the trigger; `allowed-tools`
/// pre-approves `wrk view` so it runs without a permission prompt.
fn skill_markdown() -> String {
    format!(
        "---\n\
name: wrk-view\n\
description: Open a markdown file, README, or diagram in the wrk viewer. Use when the user asks to view, open, preview, show, or visualize a markdown/text file or diagram in the terminal.\n\
allowed-tools: Bash(wrk view *)\n\
---\n\
<!-- {SKILL_INSTALL_MARKER}; safe to delete, or run `wrk uninstall-hooks` -->\n\
\n\
# View a file in the wrk viewer\n\
\n\
Run `wrk view <absolute-path>` to open a file in wrk's markdown viewer. Inside a\n\
wrk session it opens as a new tab beside the conversation; in a plain shell it\n\
opens a scrollable pager. It is read-only and never modifies the file.\n\
\n\
When the user asks to view, open, preview, show, or visualize a markdown file or\n\
diagram:\n\
\n\
1. Resolve it to an absolute path (search with grep/find if they described the\n\
   file rather than naming it).\n\
2. Run `wrk view <absolute-path>`.\n\
3. Briefly confirm it is open.\n"
    )
}

/// Write the `wrk-view` skill to `~/.claude/skills/wrk-view/SKILL.md`,
/// overwriting any prior copy (so content fixes propagate). Returns the path.
pub fn install_skill() -> Result<PathBuf> {
    install_skill_in(&claude_dir()?)
}

/// Remove the wrk-installed `wrk-view` skill. Returns the removed directory, or
/// `None` if it was absent or not one wrk wrote.
pub fn uninstall_skill() -> Result<Option<PathBuf>> {
    uninstall_skill_in(&claude_dir()?)
}

fn install_skill_in(claude_dir: &Path) -> Result<PathBuf> {
    let dir = claude_dir.join("skills/wrk-view");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("SKILL.md");
    fs::write(&path, skill_markdown()).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

fn uninstall_skill_in(claude_dir: &Path) -> Result<Option<PathBuf>> {
    let dir = claude_dir.join("skills/wrk-view");
    let path = dir.join("SKILL.md");
    if !path.exists() {
        return Ok(None);
    }
    // Only remove a skill we wrote — identified by our marker.
    let content = fs::read_to_string(&path).unwrap_or_default();
    if !content.contains(SKILL_INSTALL_MARKER) {
        return Ok(None);
    }
    fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    Ok(Some(dir))
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
    let mut settings = read_settings(&path)?;

    // Ensure top-level is an object.
    if !settings.is_object() {
        return Err(anyhow!("{} top-level is not a JSON object", path.display()));
    }
    let root = settings.as_object_mut().unwrap();
    let hooks_entry = root.entry("hooks".to_string()).or_insert_with(|| json!({}));
    if !hooks_entry.is_object() {
        return Err(anyhow!("settings.json: 'hooks' is not an object"));
    }
    let hooks_obj = hooks_entry.as_object_mut().unwrap();

    for (event, payload) in EVENTS {
        let arr_entry = hooks_obj
            .entry((*event).to_string())
            .or_insert_with(|| json!([]));
        if !arr_entry.is_array() {
            return Err(anyhow!("settings.json: hooks.{event} is not an array"));
        }
        let arr = arr_entry.as_array_mut().unwrap();
        let new_cmd = hook_command(payload);

        // If a wrk-marked entry exists, refresh its command (so re-running
        // install-hooks picks up bug fixes). Otherwise append a new entry.
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
                        .is_some_and(|c| c.contains(HOOK_MARKER));
                    if is_ours {
                        h["command"] = json!(new_cmd.clone());
                    }
                }
            }
        }
        if !found {
            arr.push(json!({
                "matcher": "",
                "hooks": [{
                    "type": "command",
                    "command": new_cmd,
                }],
            }));
        }
    }

    write_settings(&path, &settings)?;
    Ok(path)
}

pub fn uninstall_hooks() -> Result<(PathBuf, usize)> {
    let path = settings_path()?;
    if !path.exists() {
        return Ok((path, 0));
    }
    let mut settings = read_settings(&path)?;
    let Some(root) = settings.as_object_mut() else {
        return Ok((path, 0));
    };
    let Some(hooks_entry) = root.get_mut("hooks") else {
        return Ok((path, 0));
    };
    let Some(hooks_obj) = hooks_entry.as_object_mut() else {
        return Ok((path, 0));
    };

    let mut removed = 0usize;
    let event_keys: Vec<String> = EVENTS.iter().map(|(e, _)| (*e).to_string()).collect();
    for event in &event_keys {
        if let Some(arr_entry) = hooks_obj.get_mut(event)
            && let Some(arr) = arr_entry.as_array_mut()
        {
            let before = arr.len();
            arr.retain(|entry| !entry_has_marker(entry));
            removed += before - arr.len();
        }
    }
    // Drop empty event arrays we created.
    for event in &event_keys {
        let drop = hooks_obj
            .get(event)
            .and_then(|v| v.as_array())
            .is_some_and(|a| a.is_empty());
        if drop {
            hooks_obj.remove(event);
        }
    }
    let drop_hooks = hooks_obj.is_empty();
    if drop_hooks {
        root.remove("hooks");
    }

    write_settings(&path, &settings)?;
    Ok((path, removed))
}

fn entry_has_marker(entry: &Value) -> bool {
    let Some(hooks) = entry.get("hooks").and_then(|h| h.as_array()) else {
        return false;
    };
    hooks.iter().any(|h| {
        h.get("command")
            .and_then(|c| c.as_str())
            .is_some_and(|c| c.contains(HOOK_MARKER))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_markdown_has_name_and_marker() {
        let md = skill_markdown();
        assert!(md.contains("name: wrk-view"));
        assert!(md.contains("allowed-tools: Bash(wrk view *)"));
        assert!(md.contains(SKILL_INSTALL_MARKER));
        // description + triggers keep the frontmatter well under the 1536-char cap.
        assert!(md.len() < 1536);
    }

    #[test]
    fn install_then_uninstall_skill_round_trip() {
        let home = tempfile::tempdir().unwrap();
        let claude = home.path().join(".claude");

        let path = install_skill_in(&claude).unwrap();
        assert!(path.exists());
        assert!(path.ends_with("skills/wrk-view/SKILL.md"));

        let removed = uninstall_skill_in(&claude).unwrap();
        assert_eq!(removed, Some(claude.join("skills/wrk-view")));
        assert!(!path.exists());

        // Second uninstall is a no-op.
        assert_eq!(uninstall_skill_in(&claude).unwrap(), None);
    }

    #[test]
    fn uninstall_leaves_foreign_skill_untouched() {
        let home = tempfile::tempdir().unwrap();
        let claude = home.path().join(".claude");
        let dir = claude.join("skills/wrk-view");
        fs::create_dir_all(&dir).unwrap();
        // A user's own skill that happens to share the name (no wrk marker).
        fs::write(dir.join("SKILL.md"), "---\nname: wrk-view\n---\nmine\n").unwrap();

        assert_eq!(uninstall_skill_in(&claude).unwrap(), None);
        assert!(dir.join("SKILL.md").exists());
    }

    #[test]
    fn sanitize_strips_unsafe_chars() {
        assert_eq!(sanitize("foo bar/baz"), "foo_bar_baz");
        assert_eq!(sanitize("ok.name-1_2"), "ok.name-1_2");
    }

    #[test]
    fn parse_event_round_trip() {
        assert_eq!(parse_event("Stop"), Some(HookEvent::Stop));
        assert_eq!(
            parse_event("UserPromptSubmit"),
            Some(HookEvent::UserPromptSubmit)
        );
        assert_eq!(parse_event("Notification"), Some(HookEvent::Notification));
        assert_eq!(parse_event("nope"), None);
    }

    #[test]
    fn entry_marker_detection() {
        let ours = json!({
            "matcher": "",
            "hooks": [{ "type": "command", "command": "echo X > $WRK_STATUS_FILE" }]
        });
        let other = json!({
            "matcher": "",
            "hooks": [{ "type": "command", "command": "notify-send done" }]
        });
        assert!(entry_has_marker(&ours));
        assert!(!entry_has_marker(&other));
    }
}
