use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use directories::ProjectDirs;
use ratatui::style::Color;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Full command + args used to launch the Claude pane.
    /// e.g. `["claude"]` or `["steam-run", "claude"]`. wrk appends
    /// `--resume <id>` per tab from this base; do not include `--continue`
    /// (it's stripped by [`Self::claude_base`] for backwards compatibility
    /// with older configs since wrk no longer uses it — resumption is always
    /// driven by a specific session ID).
    #[serde(default = "default_claude")]
    pub claude_command: Vec<String>,

    /// Optional override for the shell pane. If unset, falls back to `$SHELL`,
    /// then `/bin/bash`.
    #[serde(default)]
    pub shell_command: Option<Vec<String>>,

    /// Optional per-key color overrides for the chrome (borders, status bar,
    /// sidebar/tab indicators). All fields are optional — anything not set
    /// keeps wrk's built-in default.
    #[serde(default)]
    pub theme: ThemeConfig,

    /// Optional shortcut overrides. Today only the `[keys.global]` namespace
    /// is consumed; the wrapper struct exists so we can grow into per-pane
    /// scopes (`[keys.projects]`, `[keys.modal]`, …) without renaming the
    /// existing top-level table.
    #[serde(default)]
    pub keys: KeyConfig,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            claude_command: default_claude(),
            shell_command: None,
            theme: ThemeConfig::default(),
            keys: KeyConfig::default(),
        }
    }
}

impl Settings {
    /// The Claude binary + wrapper args without any session flag. Strips a
    /// trailing `--continue` if present (older default; no longer used by
    /// wrk) so the caller can append `--resume <id>` as needed.
    pub fn claude_base(&self) -> Vec<String> {
        let mut cmd = self.claude_command.clone();
        if cmd.last().map(|s| s == "--continue").unwrap_or(false) {
            cmd.pop();
        }
        cmd
    }

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
    vec!["claude".into()]
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

// -----------------------------------------------------------------------------
// Theme
// -----------------------------------------------------------------------------

/// Resolved chrome colors used by the renderer. Each slot has a built-in
/// default; user overrides come from `[theme]` in `settings.toml`.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// Color for the border around the focused pane.
    pub border_focused: Color,
    /// Color for the border around an unfocused pane.
    pub border_unfocused: Color,
    /// Highlight/accent color: active project marker, active claude tab
    /// background, sidebar selection background, status bar project chip
    /// background.
    pub accent: Color,
    /// Foreground color used on top of `accent` (selection text, chip text).
    pub accent_fg: Color,
    /// Color for hint text, placeholders, and inactive labels.
    pub hint: Color,
    /// Subtle informational text in modals.
    pub info: Color,
    /// Color for error messages and "danger" modal borders.
    pub error: Color,
    /// Sidebar/tab indicator: claude finished its response (Stop hook).
    pub status_waiting: Color,
    /// Sidebar/tab indicator: claude is processing (UserPromptSubmit hook /
    /// recent PTY output).
    pub status_busy: Color,
    /// Sidebar/tab indicator: claude needs attention (Notification hook,
    /// e.g. permission prompt).
    pub status_attention: Color,
    /// Color for the `[focus]` label in the status bar.
    pub focus_indicator: Color,
}

impl Default for Theme {
    fn default() -> Self {
        // Defaults match the previous hard-coded values so users with no
        // `[theme]` section see no visual change.
        Self {
            border_focused: Color::Cyan,
            border_unfocused: Color::DarkGray,
            accent: Color::Cyan,
            accent_fg: Color::Black,
            hint: Color::DarkGray,
            info: Color::Gray,
            error: Color::Red,
            status_waiting: Color::Green,
            status_busy: Color::Yellow,
            status_attention: Color::Red,
            focus_indicator: Color::Yellow,
        }
    }
}

/// On-disk representation. Each field is optional; missing fields fall back
/// to `Theme::default()`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    pub border_focused: Option<String>,
    pub border_unfocused: Option<String>,
    pub accent: Option<String>,
    pub accent_fg: Option<String>,
    pub hint: Option<String>,
    pub info: Option<String>,
    pub error: Option<String>,
    pub status_waiting: Option<String>,
    pub status_busy: Option<String>,
    pub status_attention: Option<String>,
    pub focus_indicator: Option<String>,
}

impl ThemeConfig {
    /// Resolve the user's overrides against the built-in defaults. Invalid
    /// color strings are silently ignored — the slot keeps its default.
    pub fn resolve(&self) -> Theme {
        let mut t = Theme::default();
        let apply = |slot: &mut Color, value: &Option<String>| {
            if let Some(s) = value
                && let Some(c) = parse_color(s)
            {
                *slot = c;
            }
        };
        apply(&mut t.border_focused, &self.border_focused);
        apply(&mut t.border_unfocused, &self.border_unfocused);
        apply(&mut t.accent, &self.accent);
        apply(&mut t.accent_fg, &self.accent_fg);
        apply(&mut t.hint, &self.hint);
        apply(&mut t.info, &self.info);
        apply(&mut t.error, &self.error);
        apply(&mut t.status_waiting, &self.status_waiting);
        apply(&mut t.status_busy, &self.status_busy);
        apply(&mut t.status_attention, &self.status_attention);
        apply(&mut t.focus_indicator, &self.focus_indicator);
        t
    }
}

/// Parse a hex color (`#rrggbb` or `#rgb`) or one of the standard ratatui
/// color names (case-insensitive). Returns `None` for unrecognized input so
/// the caller can fall back to a default.
pub fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('#') {
        match rest.len() {
            6 => {
                let r = u8::from_str_radix(&rest[0..2], 16).ok()?;
                let g = u8::from_str_radix(&rest[2..4], 16).ok()?;
                let b = u8::from_str_radix(&rest[4..6], 16).ok()?;
                return Some(Color::Rgb(r, g, b));
            }
            3 => {
                let parse = |c: char| u8::from_str_radix(&c.to_string(), 16).ok();
                let mut chars = rest.chars();
                let r = parse(chars.next()?)?;
                let g = parse(chars.next()?)?;
                let b = parse(chars.next()?)?;
                // Expand 4-bit nibbles to 8-bit by duplication (e.g. f → ff).
                return Some(Color::Rgb(r * 0x11, g * 0x11, b * 0x11));
            }
            _ => return None,
        }
    }
    match s.to_ascii_lowercase().as_str() {
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "white" => Some(Color::White),
        "gray" | "grey" => Some(Color::Gray),
        "darkgray" | "darkgrey" => Some(Color::DarkGray),
        "lightred" => Some(Color::LightRed),
        "lightgreen" => Some(Color::LightGreen),
        "lightyellow" => Some(Color::LightYellow),
        "lightblue" => Some(Color::LightBlue),
        "lightmagenta" => Some(Color::LightMagenta),
        "lightcyan" => Some(Color::LightCyan),
        "reset" | "default" => Some(Color::Reset),
        _ => None,
    }
}

// -----------------------------------------------------------------------------
// Keys
// -----------------------------------------------------------------------------

use crate::keymap::GlobalAction;

/// Top-level wrapper for `[keys.<scope>]` tables. Only `global` is consumed
/// today; the wrapper exists so `[keys.projects]` / `[keys.modal]` / … can
/// be added later without renaming user configs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyConfig {
    pub global: GlobalKeysConfig,
}

/// User-facing override schema for the global keymap. Each field is optional;
/// missing fields keep the built-in default from
/// [`GlobalAction::default_binding`]. Each value is a key string parsed by
/// [`crate::keymap::parse_key`] (e.g. `"Alt+q"`, `"Ctrl+Space"`, `"F12"`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GlobalKeysConfig {
    pub quit: Option<String>,
    pub focus_projects: Option<String>,
    pub focus_claude: Option<String>,
    pub focus_shell: Option<String>,
    pub toggle_sidebar: Option<String>,
    pub shrink_claude: Option<String>,
    pub grow_claude: Option<String>,
    pub toggle_layout: Option<String>,
    pub new_claude_tab: Option<String>,
    pub close_claude_tab: Option<String>,
    pub prev_claude_tab: Option<String>,
    pub next_claude_tab: Option<String>,
    pub open_markdown: Option<String>,
    pub leader_focus_projects: Option<String>,
    pub toggle_shell_passthrough: Option<String>,
    pub open_link_picker: Option<String>,
    pub enter_select_mode: Option<String>,
    pub dump_grid: Option<String>,
}

impl crate::keymap::GlobalKeysSource for GlobalKeysConfig {
    fn get(&self, action: GlobalAction) -> Option<&str> {
        let s = match action {
            GlobalAction::Quit => &self.quit,
            GlobalAction::FocusProjects => &self.focus_projects,
            GlobalAction::FocusClaude => &self.focus_claude,
            GlobalAction::FocusShell => &self.focus_shell,
            GlobalAction::ToggleSidebar => &self.toggle_sidebar,
            GlobalAction::ShrinkClaude => &self.shrink_claude,
            GlobalAction::GrowClaude => &self.grow_claude,
            GlobalAction::ToggleLayout => &self.toggle_layout,
            GlobalAction::NewClaudeTab => &self.new_claude_tab,
            GlobalAction::CloseClaudeTab => &self.close_claude_tab,
            GlobalAction::PrevClaudeTab => &self.prev_claude_tab,
            GlobalAction::NextClaudeTab => &self.next_claude_tab,
            GlobalAction::OpenMarkdown => &self.open_markdown,
            GlobalAction::LeaderFocusProjects => &self.leader_focus_projects,
            GlobalAction::ToggleShellPassthrough => &self.toggle_shell_passthrough,
            GlobalAction::OpenLinkPicker => &self.open_link_picker,
            GlobalAction::EnterSelectMode => &self.enter_select_mode,
            GlobalAction::DumpGrid => &self.dump_grid,
        };
        s.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_six_digit_hex() {
        assert_eq!(parse_color("#1d1f21"), Some(Color::Rgb(0x1d, 0x1f, 0x21)));
    }

    #[test]
    fn parses_three_digit_hex() {
        assert_eq!(parse_color("#f0a"), Some(Color::Rgb(0xff, 0x00, 0xaa)));
    }

    #[test]
    fn parses_named_colors_case_insensitively() {
        assert_eq!(parse_color("Cyan"), Some(Color::Cyan));
        assert_eq!(parse_color("DARKGRAY"), Some(Color::DarkGray));
        assert_eq!(parse_color("darkgrey"), Some(Color::DarkGray));
        assert_eq!(parse_color("reset"), Some(Color::Reset));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_color("not-a-color"), None);
        assert_eq!(parse_color("#zzzzzz"), None);
        assert_eq!(parse_color("#12345"), None);
    }

    #[test]
    fn theme_defaults_match_builtin() {
        let t = ThemeConfig::default().resolve();
        assert_eq!(t.border_focused, Color::Cyan);
        assert_eq!(t.error, Color::Red);
    }

    #[test]
    fn theme_overrides_apply() {
        let cfg = ThemeConfig {
            border_focused: Some("#5fafff".into()),
            error: Some("lightmagenta".into()),
            ..Default::default()
        };
        let t = cfg.resolve();
        assert_eq!(t.border_focused, Color::Rgb(0x5f, 0xaf, 0xff));
        assert_eq!(t.error, Color::LightMagenta);
        // Unset slots keep defaults.
        assert_eq!(t.accent, Color::Cyan);
    }

    #[test]
    fn invalid_overrides_keep_defaults() {
        let cfg = ThemeConfig {
            border_focused: Some("notacolor".into()),
            ..Default::default()
        };
        assert_eq!(cfg.resolve().border_focused, Color::Cyan);
    }
}
