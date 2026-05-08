//! Customizable key bindings.
//!
//! The TUI's global shortcuts are looked up through a [`KeyMap`] rather than
//! hard-coded match arms. The map is built at startup by overlaying
//! `[keys.global]` entries from `settings.toml` on top of the built-in
//! defaults; missing entries keep their default binding. Status-bar hints
//! ([`KeyMap::display`]) read from the same map so customised bindings show
//! up automatically.
//!
//! Only the *global* shortcuts (those that compete with shell apps for
//! `Alt+…` / `Ctrl+Space` / `F12`) are configurable today. The TOML schema
//! is namespaced (`[keys.global]`) so per-pane scopes can layer on later
//! without renaming user configs.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A global shortcut action. The TOML key in `[keys.global]` is the lowercase
/// name (e.g. `Quit` ↔ `quit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GlobalAction {
    Quit,
    FocusProjects,
    FocusClaude,
    FocusShell,
    ToggleSidebar,
    ShrinkClaude,
    GrowClaude,
    ToggleLayout,
    NewClaudeTab,
    CloseClaudeTab,
    PrevClaudeTab,
    NextClaudeTab,
    /// Secondary "jump back to projects" binding — usable from inside a PTY
    /// where `FocusProjects`'s default Alt+1 might collide with an app.
    LeaderFocusProjects,
    ToggleShellPassthrough,
    DumpGrid,
}

impl GlobalAction {
    /// Default binding string for an action. `parse_key` of this should
    /// always succeed — the unit tests guard that.
    pub fn default_binding(self) -> &'static str {
        match self {
            Self::Quit => "Alt+q",
            Self::FocusProjects => "Alt+1",
            Self::FocusClaude => "Alt+2",
            Self::FocusShell => "Alt+3",
            Self::ToggleSidebar => "Alt+0",
            Self::ShrinkClaude => "Alt+h",
            Self::GrowClaude => "Alt+l",
            Self::ToggleLayout => "Alt+t",
            Self::NewClaudeTab => "Alt+n",
            Self::CloseClaudeTab => "Alt+w",
            Self::PrevClaudeTab => "Alt+<",
            Self::NextClaudeTab => "Alt+>",
            Self::LeaderFocusProjects => "Ctrl+Space",
            Self::ToggleShellPassthrough => "F12",
            Self::DumpGrid => "Alt+x",
        }
    }

    /// Iterate all actions in a fixed order. Used to seed defaults and to
    /// build the keymap from a config struct.
    pub const ALL: &'static [Self] = &[
        Self::Quit,
        Self::FocusProjects,
        Self::FocusClaude,
        Self::FocusShell,
        Self::ToggleSidebar,
        Self::ShrinkClaude,
        Self::GrowClaude,
        Self::ToggleLayout,
        Self::NewClaudeTab,
        Self::CloseClaudeTab,
        Self::PrevClaudeTab,
        Self::NextClaudeTab,
        Self::LeaderFocusProjects,
        Self::ToggleShellPassthrough,
        Self::DumpGrid,
    ];
}

/// Resolved global key bindings.
#[derive(Debug, Clone, Default)]
pub struct KeyMap {
    /// Maps a normalized [`KeyEvent`] to its bound action. "Normalized" means
    /// `kind = Press` and `state = NONE` so it matches what the event loop
    /// sees.
    by_key: HashMap<KeyEvent, GlobalAction>,
    /// Reverse map, used by [`Self::display`] to format hints.
    by_action: HashMap<GlobalAction, KeyEvent>,
}

impl KeyMap {
    /// Build a keymap from the user's overrides layered on top of defaults.
    /// Returns the keymap plus a list of human-readable warnings (invalid
    /// strings, conflicts) for the caller to surface in the UI.
    pub fn build(overrides: &impl GlobalKeysSource) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let mut by_key: HashMap<KeyEvent, GlobalAction> = HashMap::new();
        let mut by_action: HashMap<GlobalAction, KeyEvent> = HashMap::new();

        for &action in GlobalAction::ALL {
            let resolved = match overrides.get(action) {
                Some(s) => match parse_key(s) {
                    Some(k) => k,
                    None => {
                        warnings.push(format!(
                            "keys.global.{}: invalid key '{s}', using default '{}'",
                            action_name(action),
                            action.default_binding()
                        ));
                        parse_key(action.default_binding()).expect("default binding parses")
                    }
                },
                None => parse_key(action.default_binding()).expect("default binding parses"),
            };
            // Conflict: this key is already bound to a different action.
            if let Some(&prev) = by_key.get(&resolved) {
                warnings.push(format!(
                    "keys.global.{}: '{}' already bound to {}, override wins",
                    action_name(action),
                    display_key(&resolved),
                    action_name(prev),
                ));
                by_action.remove(&prev);
            }
            by_key.insert(resolved, action);
            by_action.insert(action, resolved);
        }

        (Self { by_key, by_action }, warnings)
    }

    /// Look up the action bound to `key`, normalizing kind/state first so the
    /// match doesn't depend on whether the event was a Press, Repeat, or
    /// Release (we only see Press in the loop, but be explicit).
    pub fn lookup(&self, key: &KeyEvent) -> Option<GlobalAction> {
        let normal = KeyEvent::new(key.code, key.modifiers);
        self.by_key.get(&normal).copied()
    }

    /// Render the binding for `action` as a human-readable string (e.g.
    /// `"Alt+q"`). Returns `"?"` if the action somehow has no binding —
    /// shouldn't happen for any action in [`GlobalAction::ALL`].
    pub fn display(&self, action: GlobalAction) -> String {
        self.by_action
            .get(&action)
            .map(display_key)
            .unwrap_or_else(|| "?".to_string())
    }
}

/// Source of per-action overrides. Decouples [`KeyMap::build`] from the
/// concrete TOML struct so tests can pass a `HashMap` directly.
pub trait GlobalKeysSource {
    fn get(&self, action: GlobalAction) -> Option<&str>;
}

/// Parse a key string like `"Alt+q"`, `"Ctrl+Space"`, `"F12"`, `"Alt+<"`.
///
/// Modifiers are case-insensitive: `Ctrl`/`Control`, `Alt`/`Meta`, `Shift`,
/// `Super`/`Cmd`/`Win`. The trailing segment is the key — single chars,
/// `F1`–`F24`, or named keys (`Space`, `Enter`, `Tab`, `Esc`, `Backspace`,
/// `Delete`, `Insert`, `Up`/`Down`/`Left`/`Right`, `Home`, `End`, `PageUp`,
/// `PageDown`).
///
/// `Shift+<letter>` is normalized to the uppercase letter with no SHIFT
/// modifier so it matches what crossterm reports under
/// `KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS` (which wrk enables).
pub fn parse_key(s: &str) -> Option<KeyEvent> {
    let parts: Vec<&str> = s.split('+').map(|p| p.trim()).collect();
    let (key_str, mod_strs) = parts.split_last()?;
    if key_str.is_empty() {
        return None;
    }

    let mut mods = KeyModifiers::NONE;
    for m in mod_strs {
        match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
            "alt" | "meta" => mods |= KeyModifiers::ALT,
            "shift" => mods |= KeyModifiers::SHIFT,
            "super" | "cmd" | "win" => mods |= KeyModifiers::SUPER,
            _ => return None,
        }
    }

    let code = match key_str.to_ascii_lowercase().as_str() {
        "space" => KeyCode::Char(' '),
        "enter" | "return" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "esc" | "escape" => KeyCode::Esc,
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        s if s.starts_with('f') && s.len() > 1 => {
            let n: u8 = s[1..].parse().ok()?;
            if !(1..=24).contains(&n) {
                return None;
            }
            KeyCode::F(n)
        }
        _ => {
            let mut chars = key_str.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            // `Shift+a` → `A` (no SHIFT). Crossterm reports shifted printable
            // chars this way under REPORT_ALTERNATE_KEYS, so the binding
            // needs to match.
            if mods.contains(KeyModifiers::SHIFT) && c.is_ascii_alphabetic() {
                mods.remove(KeyModifiers::SHIFT);
                KeyCode::Char(c.to_ascii_uppercase())
            } else {
                KeyCode::Char(c)
            }
        }
    };

    Some(KeyEvent::new(code, mods))
}

/// Render a [`KeyEvent`] as a human-readable string in the same shape that
/// `parse_key` accepts. Modifier order is `Ctrl+Alt+Shift+Super+Key` so
/// hints stay consistent.
pub fn display_key(k: &KeyEvent) -> String {
    let mut parts: Vec<String> = Vec::new();
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("Ctrl".into());
    }
    if k.modifiers.contains(KeyModifiers::ALT) {
        parts.push("Alt".into());
    }
    if k.modifiers.contains(KeyModifiers::SHIFT) {
        parts.push("Shift".into());
    }
    if k.modifiers.contains(KeyModifiers::SUPER) {
        parts.push("Super".into());
    }
    let code = match k.code {
        KeyCode::Char(' ') => "Space".into(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::BackTab => "BackTab".into(),
        KeyCode::Esc => "Esc".into(),
        KeyCode::Backspace => "Backspace".into(),
        KeyCode::Delete => "Delete".into(),
        KeyCode::Insert => "Insert".into(),
        KeyCode::Up => "Up".into(),
        KeyCode::Down => "Down".into(),
        KeyCode::Left => "Left".into(),
        KeyCode::Right => "Right".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::PageUp => "PageUp".into(),
        KeyCode::PageDown => "PageDown".into(),
        KeyCode::F(n) => format!("F{n}"),
        _ => "?".into(),
    };
    parts.push(code);
    parts.join("+")
}

fn action_name(a: GlobalAction) -> &'static str {
    match a {
        GlobalAction::Quit => "quit",
        GlobalAction::FocusProjects => "focus_projects",
        GlobalAction::FocusClaude => "focus_claude",
        GlobalAction::FocusShell => "focus_shell",
        GlobalAction::ToggleSidebar => "toggle_sidebar",
        GlobalAction::ShrinkClaude => "shrink_claude",
        GlobalAction::GrowClaude => "grow_claude",
        GlobalAction::ToggleLayout => "toggle_layout",
        GlobalAction::NewClaudeTab => "new_claude_tab",
        GlobalAction::CloseClaudeTab => "close_claude_tab",
        GlobalAction::PrevClaudeTab => "prev_claude_tab",
        GlobalAction::NextClaudeTab => "next_claude_tab",
        GlobalAction::LeaderFocusProjects => "leader_focus_projects",
        GlobalAction::ToggleShellPassthrough => "toggle_shell_passthrough",
        GlobalAction::DumpGrid => "dump_grid",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_all_parse() {
        for &a in GlobalAction::ALL {
            assert!(
                parse_key(a.default_binding()).is_some(),
                "default for {:?} ('{}') failed to parse",
                a,
                a.default_binding()
            );
        }
    }

    #[test]
    fn defaults_have_no_conflicts() {
        struct Empty;
        impl GlobalKeysSource for Empty {
            fn get(&self, _: GlobalAction) -> Option<&str> {
                None
            }
        }
        let (_, warnings) = KeyMap::build(&Empty);
        assert!(
            warnings.is_empty(),
            "default keymap conflicts: {warnings:?}"
        );
    }

    #[test]
    fn parse_alt_q() {
        let k = parse_key("Alt+q").unwrap();
        assert_eq!(k.code, KeyCode::Char('q'));
        assert!(k.modifiers.contains(KeyModifiers::ALT));
    }

    #[test]
    fn parse_ctrl_space() {
        let k = parse_key("Ctrl+Space").unwrap();
        assert_eq!(k.code, KeyCode::Char(' '));
        assert!(k.modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn parse_f12() {
        let k = parse_key("F12").unwrap();
        assert_eq!(k.code, KeyCode::F(12));
        assert_eq!(k.modifiers, KeyModifiers::NONE);
    }

    #[test]
    fn parse_alt_lt() {
        // Shifted printable char; no SHIFT in the result.
        let k = parse_key("Alt+<").unwrap();
        assert_eq!(k.code, KeyCode::Char('<'));
        assert_eq!(k.modifiers, KeyModifiers::ALT);
    }

    #[test]
    fn shift_letter_normalizes_to_uppercase() {
        let k = parse_key("Shift+a").unwrap();
        assert_eq!(k.code, KeyCode::Char('A'));
        assert_eq!(k.modifiers, KeyModifiers::NONE);
    }

    #[test]
    fn shift_fkey_keeps_modifier() {
        let k = parse_key("Shift+F1").unwrap();
        assert_eq!(k.code, KeyCode::F(1));
        assert!(k.modifiers.contains(KeyModifiers::SHIFT));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_key("notakey").is_none());
        assert!(parse_key("Foo+q").is_none());
        assert!(parse_key("Alt+ab").is_none());
        assert!(parse_key("F0").is_none());
        assert!(parse_key("F25").is_none());
    }

    #[test]
    fn override_takes_effect_and_warns_on_conflict() {
        // Bind Quit to Alt+1 — collides with FocusProjects.
        let mut map: HashMap<GlobalAction, String> = HashMap::new();
        map.insert(GlobalAction::Quit, "Alt+1".into());
        struct H(HashMap<GlobalAction, String>);
        impl GlobalKeysSource for H {
            fn get(&self, a: GlobalAction) -> Option<&str> {
                self.0.get(&a).map(String::as_str)
            }
        }
        let (km, warnings) = KeyMap::build(&H(map));
        assert!(warnings.iter().any(|w| w.contains("focus_projects")));
        assert_eq!(
            km.lookup(&parse_key("Alt+1").unwrap()),
            Some(GlobalAction::FocusProjects)
        );
    }

    #[test]
    fn display_round_trips_basic() {
        for s in ["Alt+q", "Ctrl+Space", "F12", "Alt+<", "Alt+0"] {
            let k = parse_key(s).unwrap();
            assert_eq!(display_key(&k), s);
        }
    }
}
