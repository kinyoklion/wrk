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
            modified,
        });
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    out
}

fn sessions_dir(project_path: &Path) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let key = path_to_claude_key(project_path);
    Some(PathBuf::from(home).join(".claude/projects").join(key))
}

fn path_to_claude_key(path: &Path) -> String {
    path.to_string_lossy().replace('/', "-")
}
