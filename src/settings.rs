use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Full command + args used to launch the Claude pane.
    /// e.g. `["claude", "--continue"]` or `["steam-run", "claude", "--continue"]`.
    #[serde(default = "default_claude")]
    pub claude_command: Vec<String>,

    /// Optional override for the shell pane. If unset, falls back to `$SHELL`,
    /// then `/bin/bash`.
    #[serde(default)]
    pub shell_command: Option<Vec<String>>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            claude_command: default_claude(),
            shell_command: None,
        }
    }
}

impl Settings {
    pub fn shell(&self) -> Vec<String> {
        if let Some(cmd) = &self.shell_command
            && !cmd.is_empty()
        {
            return cmd.clone();
        }
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        vec![shell]
    }
}

fn default_claude() -> Vec<String> {
    vec!["claude".into(), "--continue".into()]
}

pub fn settings_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("", "", "wrk")
        .ok_or_else(|| anyhow!("could not determine config directory"))?;
    Ok(dirs.config_dir().join("settings.toml"))
}

pub fn load() -> Result<Settings> {
    let path = settings_path()?;
    if !path.exists() {
        return Ok(Settings::default());
    }
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let settings: Settings =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(settings)
}
