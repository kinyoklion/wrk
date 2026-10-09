use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::settings::HarnessKind;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LayoutMode {
    Split,
    #[default]
    Tabbed,
}

/// A named pointer to a specific Claude session.
///
/// Stored in `projects.toml` so wrk can resume the right conversation when
/// the user opens a project. `session_id` corresponds to the UUID in
/// `~/.claude/projects/<path>/<uuid>.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionRef {
    pub name: String,
    /// Agent session id used to resume the same conversation. For Claude it's a
    /// UUID wrk mints at spawn time (`claude --session-id <uuid>`); for Kimi it's
    /// the id the agent assigns, learned from its `SessionStart`/status hooks and
    /// persisted here. When present, wrk resumes (`claude --resume <id>` /
    /// `kimi --session <id>`); when absent, a fresh session is started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Which coding-agent harness this tab runs. Defaults to Claude and is
    /// omitted from the TOML when Claude, so claude-only configs are unchanged.
    #[serde(default, skip_serializing_if = "HarnessKind::is_claude")]
    pub harness: HarnessKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Project {
    pub name: String,
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Per-project preferred layout mode (split panes vs tabbed). Persisted
    /// in projects.toml as `layout = "split"` / `"tabbed"`. None → default.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "layout")]
    pub layout_mode: Option<LayoutMode>,
    /// Per-project shell-pane passthrough state. When `Some(true)`, wrk's
    /// global Alt+… / Ctrl+Space shortcuts are not intercepted while the
    /// shell pane is focused — every key (except F12, which toggles this
    /// flag) is forwarded straight to the PTY. Persisted in projects.toml
    /// as `passthrough = true`. None → default (false).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "passthrough"
    )]
    pub shell_passthrough: Option<bool>,
    /// Named Claude sessions associated with this project. When empty wrk
    /// spawns one fresh new Claude session on open and records its ID here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claude_sessions: Vec<SessionRef>,
    /// Every agent session wrk has hosted for this project, session id → tab
    /// name. Unlike `claude_sessions` (the live tab list) entries survive their
    /// tab being closed or the tab switching sessions, so the new-tab picker can
    /// label past sessions. Backfilled from `claude_sessions` on load.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub session_names: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectStore {
    #[serde(default, rename = "project")]
    pub projects: Vec<Project>,
}

impl Project {
    /// Record the name of every live tab's session in `session_names`. A live
    /// tab's current name wins over an older recorded one.
    pub fn remember_live_session_names(&mut self) {
        for sr in &self.claude_sessions {
            if let Some(id) = &sr.session_id {
                self.session_names.insert(id.clone(), sr.name.clone());
            }
        }
    }
}

impl ProjectStore {
    pub fn find(&self, name: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.name == name)
    }

    pub fn add(&mut self, project: Project) -> Result<()> {
        if self.find(&project.name).is_some() {
            return Err(anyhow!("project '{}' already exists", project.name));
        }
        self.projects.push(project);
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Result<Project> {
        let pos = self
            .projects
            .iter()
            .position(|p| p.name == name)
            .ok_or_else(|| anyhow!("project '{name}' not found"))?;
        Ok(self.projects.remove(pos))
    }
}

pub fn config_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("", "", "wrk")
        .ok_or_else(|| anyhow!("could not determine config directory"))?;
    Ok(dirs.config_dir().join("projects.toml"))
}

pub fn load() -> Result<ProjectStore> {
    let path = config_path()?;
    load_from(&path)
}

pub fn load_from(path: &Path) -> Result<ProjectStore> {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating config dir {}", parent.display()))?;
        }
        fs::write(path, "").with_context(|| format!("creating {}", path.display()))?;
        return Ok(ProjectStore::default());
    }
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut store: ProjectStore =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    for project in &mut store.projects {
        project.remember_live_session_names();
    }
    Ok(store)
}

pub fn save(store: &ProjectStore) -> Result<()> {
    let path = config_path()?;
    save_to(store, &path)
}

pub fn save_to(store: &ProjectStore, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(store).context("serializing project store")?;
    fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn round_trip_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("projects.toml");
        let store = load_from(&path).unwrap();
        assert!(store.projects.is_empty());
        save_to(&store, &path).unwrap();
        let again = load_from(&path).unwrap();
        assert_eq!(store, again);
    }

    #[test]
    fn round_trip_with_projects() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("projects.toml");
        let mut store = ProjectStore::default();
        store
            .add(Project {
                name: "alpha".into(),
                path: PathBuf::from("/tmp/alpha"),
                tags: vec![],
                layout_mode: None,
                shell_passthrough: None,
                claude_sessions: vec![],
                session_names: BTreeMap::new(),
            })
            .unwrap();
        store
            .add(Project {
                name: "beta".into(),
                path: PathBuf::from("/tmp/beta"),
                tags: vec!["work".into()],
                layout_mode: Some(LayoutMode::Tabbed),
                shell_passthrough: Some(true),
                claude_sessions: vec![],
                session_names: BTreeMap::new(),
            })
            .unwrap();
        save_to(&store, &path).unwrap();
        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded, store);
    }

    #[test]
    fn duplicate_add_rejected() {
        let mut store = ProjectStore::default();
        let p = Project {
            name: "x".into(),
            path: PathBuf::from("/x"),
            tags: vec![],
            layout_mode: None,
            shell_passthrough: None,
            claude_sessions: vec![],
            session_names: BTreeMap::new(),
        };
        store.add(p.clone()).unwrap();
        assert!(store.add(p).is_err());
    }

    #[test]
    fn remove_missing_errors() {
        let mut store = ProjectStore::default();
        assert!(store.remove("nope").is_err());
    }

    /// Regression: a single tab named "claude" with no session_id used to be
    /// erased by a "default-tab" special case in `App::persist_claude_sessions`,
    /// which made closing the only tab respawn a fresh `--continue` tab on
    /// next open. The special case is gone; the entry must round-trip.
    #[test]
    fn round_trip_single_unnamed_session() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("projects.toml");
        let mut store = ProjectStore::default();
        store
            .add(Project {
                name: "p".into(),
                path: PathBuf::from("/tmp/p"),
                tags: vec![],
                layout_mode: None,
                shell_passthrough: None,
                claude_sessions: vec![SessionRef {
                    name: "claude".into(),
                    session_id: None,
                    harness: HarnessKind::Claude,
                }],
                session_names: BTreeMap::new(),
            })
            .unwrap();
        save_to(&store, &path).unwrap();
        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded, store);
        assert_eq!(loaded.projects[0].claude_sessions.len(), 1);
    }

    /// A `harness` field round-trips, defaults to Claude when absent, and is
    /// omitted from the serialized TOML for Claude tabs (schema unchanged for
    /// claude-only configs) but written for non-Claude ones.
    #[test]
    fn round_trip_with_harness() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("projects.toml");
        let mut store = ProjectStore::default();
        store
            .add(Project {
                name: "p".into(),
                path: PathBuf::from("/tmp/p"),
                tags: vec![],
                layout_mode: None,
                shell_passthrough: None,
                claude_sessions: vec![
                    SessionRef {
                        name: "c".into(),
                        session_id: Some("uuid-1".into()),
                        harness: HarnessKind::Claude,
                    },
                    SessionRef {
                        name: "k".into(),
                        session_id: Some("session_abc".into()),
                        harness: HarnessKind::Kimi,
                    },
                ],
                session_names: BTreeMap::from([
                    ("uuid-1".into(), "c".into()),
                    ("session_abc".into(), "k".into()),
                ]),
            })
            .unwrap();
        save_to(&store, &path).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        // Claude tab omits the key; kimi tab records it.
        assert!(text.contains(r#"harness = "kimi""#));
        assert_eq!(text.matches("harness").count(), 1);

        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded, store);
        // A hand-written entry with no `harness` key defaults to Claude.
        let back: SessionRef = toml::from_str("name = \"x\"\n").unwrap();
        assert_eq!(back.harness, HarnessKind::Claude);
    }

    /// Names of closed tabs' sessions survive a save/load, and a config written
    /// before `session_names` existed is backfilled from its live tabs.
    #[test]
    fn session_names_round_trip_and_backfill() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("projects.toml");
        fs::write(
            &path,
            "[[project]]\nname = \"p\"\npath = \"/tmp/p\"\n\n\
             [[project.claude_sessions]]\nname = \"live\"\nsession_id = \"id-live\"\n",
        )
        .unwrap();
        let mut store = load_from(&path).unwrap();
        assert_eq!(
            store.projects[0]
                .session_names
                .get("id-live")
                .map(String::as_str),
            Some("live")
        );

        store.projects[0]
            .session_names
            .insert("id-closed".into(), "closed".into());
        save_to(&store, &path).unwrap();
        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded, store);
        assert_eq!(loaded.projects[0].session_names.len(), 2);
    }
}
