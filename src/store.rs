use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LayoutMode {
    #[default]
    Split,
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
    /// Claude session UUID. When present, wrk launches `claude --resume <id>`.
    /// When absent, wrk launches `claude --continue` (most-recent session).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
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
    /// spawns one default `claude --continue` tab (backwards-compatible).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claude_sessions: Vec<SessionRef>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectStore {
    #[serde(default, rename = "project")]
    pub projects: Vec<Project>,
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
    let store: ProjectStore =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
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
        };
        store.add(p.clone()).unwrap();
        assert!(store.add(p).is_err());
    }

    #[test]
    fn remove_missing_errors() {
        let mut store = ProjectStore::default();
        assert!(store.remove("nope").is_err());
    }
}
