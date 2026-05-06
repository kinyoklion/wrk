use std::collections::HashMap;

/// Discover Claude sessions stored on disk for a given project directory.
///
/// Claude Code stores session history under `~/.claude/projects/<key>/` where
/// `<key>` is the absolute path with every `/` replaced by `-`.  Each session
/// is a `<uuid>.jsonl` file whose first line is a JSON object with a
/// `"sessionId"` field.
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct DiscoveredSession {
    pub session_id: String,
    /// Display name — set to the stored tab name if one is known, otherwise None.
    pub name: Option<String>,
    /// Wall-clock time of the last write to the session file — used for
    /// display ordering (newest first).
    pub modified: SystemTime,
}

/// Returns sessions found on disk for `project_path`, newest first.
pub fn discover_sessions(project_path: &Path) -> Vec<DiscoveredSession> {
    let Some(dir) = sessions_dir(project_path) else {
        return vec![];
    };
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let session_id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_owned(),
            None => continue,
        };
        // Skip the memory directory (not a session file).
        if session_id == "memory" {
            continue;
        }
        let modified = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        out.push(DiscoveredSession {
            session_id,
            name: None,
            modified,
        });
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.modified));
    out
}

/// Like `discover_sessions` but annotates each entry with its stored display
/// name from `known` (mapping session_id → name).
pub fn discover_sessions_named(
    project_path: &Path,
    known: &HashMap<String, String>,
) -> Vec<DiscoveredSession> {
    let mut sessions = discover_sessions(project_path);
    for s in &mut sessions {
        s.name = known.get(&s.session_id).cloned();
    }
    sessions
}

/// Returns the session ID of the newest session file that was modified
/// strictly after `since`, or `None` if no such file exists yet.
///
/// Used after spawning `claude` (no args) to capture the newly-created
/// session ID from the filesystem.
pub fn find_session_created_after(project_path: &Path, since: SystemTime) -> Option<String> {
    let sessions = discover_sessions(project_path);
    // sessions is sorted newest-first; find the first one modified after `since`
    sessions
        .into_iter()
        .find(|s| s.modified > since)
        .map(|s| s.session_id)
}

fn sessions_dir(project_path: &Path) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let key = path_to_claude_key(project_path);
    Some(PathBuf::from(home).join(".claude/projects").join(key))
}

fn path_to_claude_key(path: &Path) -> String {
    path.to_string_lossy().replace('/', "-")
}
