//! Widgets: terminal programs wrk summons into a floating overlay
//! (`[widget.<name>]` in `settings.toml`). wrk only hosts the program in a
//! PTY — the program draws its own UI (a shell script with `fzf`, a full
//! TUI, …) and drives wrk back through `wrk api` over `WRK_SOCK`: list the
//! sessions, type into one, show one, post notifications (see `src/api.rs`).
//!
//! A widget keeps running while hidden, so it keeps its state between
//! summons and can post notifications in the background. A `project`-scoped
//! widget (the default) runs once per project, in that project's directory; a
//! `global` one runs once, and can `autostart` hidden when wrk starts.
//!
//! When the program exits cleanly its overlay just closes (a one-shot picker
//! can simply exit when it's done). When it fails while on screen, the overlay
//! stays up showing its last output and the exit status until a key closes
//! it, so the error can be read.

use crossterm::event::KeyEvent;
use ratatui::layout::Rect;

use crate::keymap::GlobalAction;
use crate::pane::terminal::PtyPane;
use crate::settings::{WidgetConfig, WidgetScope};
use crate::ui::ModalState;
use crate::ui::modal::UrlPickerModal;
use crate::{App, inset, push_error};

/// Identifies a running widget: its name and, for a project-scoped widget, the
/// project it runs for.
pub type WidgetKey = (String, Option<String>);

pub struct WidgetInstance {
    pub key: WidgetKey,
    pub pane: PtyPane,
    /// Set when the program failed while on screen: why, shown on the overlay
    /// (which keeps its last output) until a key closes it.
    pub exited: Option<String>,
}

/// The running widgets and which one is on screen.
#[derive(Default)]
pub struct Widgets {
    pub instances: Vec<WidgetInstance>,
    pub shown: Option<WidgetKey>,
}

impl Widgets {
    pub fn get(&self, key: &WidgetKey) -> Option<&WidgetInstance> {
        self.instances.iter().find(|w| &w.key == key)
    }

    /// The widget on screen.
    pub fn shown(&self) -> Option<&WidgetInstance> {
        self.shown.as_ref().and_then(|k| self.get(k))
    }

    pub fn shown_mut(&mut self) -> Option<&mut WidgetInstance> {
        let key = self.shown.as_ref()?;
        self.instances.iter_mut().find(|w| &w.key == key)
    }
}

/// The overlay's bordered rect for the body area: centered, four fifths of
/// each dimension, but at least 40×10 where the screen allows.
pub fn overlay_rect(body: Rect) -> Rect {
    let scale = |n: u16, min: u16| ((u32::from(n) * 4 / 5) as u16).max(min.min(n));
    let width = scale(body.width, 40);
    let height = scale(body.height, 10);
    Rect {
        x: body.x + (body.width - width) / 2,
        y: body.y + (body.height - height) / 2,
        width,
        height,
    }
}

/// Where the widget's terminal goes: the overlay inside its border.
pub fn overlay_inner(body: Rect) -> Rect {
    inset(overlay_rect(body))
}

/// The summon keys from `[widget.<name>] key`, with a warning for each that
/// doesn't parse or is already a global shortcut (which keeps priority).
pub fn summon_keys(
    widgets: &std::collections::BTreeMap<String, WidgetConfig>,
    keymap: &crate::keymap::KeyMap,
) -> (std::collections::HashMap<KeyEvent, String>, Vec<String>) {
    let mut keys = std::collections::HashMap::new();
    let mut warnings = Vec::new();
    for (name, cfg) in widgets {
        let Some(s) = &cfg.key else {
            continue;
        };
        match crate::keymap::parse_key(s) {
            None => warnings.push(format!("widget.{name}.key: invalid key '{s}'")),
            Some(k) if keymap.lookup(&k).is_some() => warnings.push(format!(
                "widget.{name}.key: '{s}' is already a global shortcut; ignored"
            )),
            Some(k) => {
                if let Some(prev) = keys.insert(k, name.clone()) {
                    warnings.push(format!(
                        "widget.{name}.key: '{s}' is also widget.{prev}'s; {name} wins"
                    ));
                }
            }
        }
    }
    (keys, warnings)
}

impl App {
    /// The configured widgets that can run (they have a command), sorted.
    pub(crate) fn widget_names(&self) -> Vec<String> {
        self.settings
            .widget
            .iter()
            .filter(|(_, cfg)| !cfg.command.is_empty())
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Which instance of widget `name` the user means right now: the active
    /// project's, for a project-scoped widget.
    fn widget_key_for(&self, name: &str, cfg: &WidgetConfig) -> WidgetKey {
        let project = match cfg.scope {
            WidgetScope::Project => self.active_project_name.clone(),
            WidgetScope::Global => None,
        };
        (name.to_string(), project)
    }

    /// Show widget `name`, starting it if it isn't running.
    pub(crate) fn show_widget(&mut self, name: &str, body: Rect) -> Result<(), String> {
        let cfg = self
            .settings
            .widget
            .get(name)
            .cloned()
            .ok_or_else(|| format!("no widget named '{name}'"))?;
        if cfg.command.is_empty() {
            return Err(format!("widget '{name}' has no command"));
        }
        let key = self.widget_key_for(name, &cfg);
        // A failed run left on screen is replaced by a fresh start.
        self.widgets
            .instances
            .retain(|w| w.key != key || w.exited.is_none());
        if self.widgets.get(&key).is_none() {
            let pane = self.spawn_widget(name, &cfg, key.1.as_deref(), overlay_inner(body))?;
            self.widgets.instances.push(WidgetInstance {
                key: key.clone(),
                pane,
                exited: None,
            });
        }
        self.widgets.shown = Some(key);
        Ok(())
    }

    fn spawn_widget(
        &self,
        name: &str,
        cfg: &WidgetConfig,
        project: Option<&str>,
        inner: Rect,
    ) -> Result<PtyPane, String> {
        let cwd = match project.and_then(|p| self.project(p)) {
            Some(p) => p.path.clone(),
            None => cfg
                .cwd
                .as_deref()
                .map(crate::expand_tilde)
                .unwrap_or_else(crate::remote::home_dir),
        };
        let mut env = vec![("WRK_WIDGET".to_string(), name.to_string())];
        if let Some(sock) = &self.socket_path {
            env.push(("WRK_SOCK".into(), sock.to_string_lossy().into_owned()));
        }
        if let Ok(exe) = std::env::current_exe() {
            env.push(("WRK_BIN".into(), exe.to_string_lossy().into_owned()));
        }
        if let Some(p) = project {
            env.push(("WRK_PROJECT".into(), p.to_string()));
        }
        PtyPane::spawn(
            &cfg.command,
            &cwd,
            inner.height.max(1),
            inner.width.max(1),
            &env,
        )
        .map_err(|e| format!("widget '{name}': {e:#}"))
    }

    /// Take the widget off screen; it keeps running in the background. One
    /// that failed is dropped instead (its next summon starts it afresh).
    pub(crate) fn hide_widget(&mut self) {
        if let Some(key) = self.widgets.shown.take() {
            self.widgets
                .instances
                .retain(|w| w.key != key || w.exited.is_none());
        }
    }

    /// The `Widgets` shortcut: hide the widget on screen; otherwise summon
    /// the only widget, or offer a picker when there are several.
    pub(crate) fn toggle_widgets(&mut self, body: Rect) {
        if self.widgets.shown.is_some() {
            self.hide_widget();
            return;
        }
        let names = self.widget_names();
        match names.as_slice() {
            [] => {
                self.info =
                    Some("no widgets configured: add a [widget.<name>] to settings.toml".into())
            }
            [only] => {
                let only = only.clone();
                if let Err(e) = self.show_widget(&only, body) {
                    push_error(&mut self.error, e);
                }
            }
            _ => {
                let labels = names
                    .iter()
                    .map(|n| self.widget_label(n))
                    .collect::<Vec<_>>();
                self.modal = Some(ModalState::WidgetPicker(UrlPickerModal::titled(
                    labels,
                    " widgets ",
                )));
            }
        }
    }

    /// How the picker lists widget `name`: its title, with the name when they
    /// differ (the picker maps a choice back by position).
    fn widget_label(&self, name: &str) -> String {
        match self
            .settings
            .widget
            .get(name)
            .and_then(|c| c.title.as_deref())
        {
            Some(title) if title != name => format!("{title}  ({name})"),
            _ => name.to_string(),
        }
    }

    /// The overlay title for the widget on screen.
    pub(crate) fn widget_title(&self, key: &WidgetKey) -> String {
        let title = self
            .settings
            .widget
            .get(&key.0)
            .and_then(|c| c.title.clone())
            .unwrap_or_else(|| key.0.clone());
        match &key.1 {
            Some(project) => format!("{title} · {project}"),
            None => title,
        }
    }

    /// Start the `autostart` global widgets, hidden.
    pub(crate) fn autostart_widgets(&mut self, body: Rect) {
        let names: Vec<String> = self
            .settings
            .widget
            .iter()
            .filter(|(_, c)| c.autostart && c.scope == WidgetScope::Global && !c.command.is_empty())
            .map(|(n, _)| n.clone())
            .collect();
        for name in names {
            let Some(cfg) = self.settings.widget.get(&name).cloned() else {
                continue;
            };
            match self.spawn_widget(&name, &cfg, None, overlay_inner(body)) {
                Ok(pane) => self.widgets.instances.push(WidgetInstance {
                    key: (name, None),
                    pane,
                    exited: None,
                }),
                Err(e) => push_error(&mut self.error, e),
            }
        }
    }

    /// Per tick: fit the shown widget to the overlay, and reap programs that
    /// exited — a clean exit closes quietly; a failure on screen stays up with
    /// the reason; a failure in the background is reported in the status bar.
    pub(crate) fn tick_widgets(&mut self, body: Rect) {
        let inner = overlay_inner(body);
        let shown = self.widgets.shown.clone();
        let mut errors = Vec::new();
        self.widgets.instances.retain_mut(|w| {
            if w.exited.is_some() {
                return true;
            }
            let on_screen = shown.as_ref() == Some(&w.key);
            if on_screen {
                let _ = w.pane.resize(inner.height.max(1), inner.width.max(1));
            }
            let Some(status) = w.pane.exit_status() else {
                return true;
            };
            if status.success() {
                return false;
            }
            let why = format!("exited with status {}", status.exit_code());
            if on_screen {
                w.exited = Some(why);
                true
            } else {
                errors.push(format!("widget '{}' {why}", w.key.0));
                false
            }
        });
        if let Some(key) = &shown
            && self.widgets.get(key).is_none()
        {
            self.widgets.shown = None;
        }
        for e in errors {
            push_error(&mut self.error, e);
        }
    }

    /// Handle a key while a widget is on screen: the `Widgets` shortcut or the
    /// widget's own key hides it, another widget's key switches to that one,
    /// the notification shortcuts act on a banner when one is up, and
    /// everything else goes to the widget's program. After a failed run any
    /// key closes the overlay.
    pub(crate) fn widget_key(&mut self, key: KeyEvent, body: Rect) {
        let action = self.keymap.lookup(&key);
        let summon = self
            .widget_keys
            .get(&KeyEvent::new(key.code, key.modifiers))
            .cloned();
        let shown_name = self.widgets.shown.as_ref().map(|k| k.0.clone());
        if action == Some(GlobalAction::Widgets) || (summon.is_some() && summon == shown_name) {
            self.hide_widget();
            return;
        }
        if let Some(other) = summon {
            if let Err(e) = self.show_widget(&other, body) {
                push_error(&mut self.error, e);
            }
            return;
        }
        if matches!(
            action,
            Some(GlobalAction::OpenNotification | GlobalAction::ClearNotification)
        ) && !self.notifications.is_empty()
        {
            if action == Some(GlobalAction::OpenNotification) {
                self.open_notification(body);
            } else {
                self.notifications.pop_newest();
            }
            return;
        }
        let Some(w) = self.widgets.shown_mut() else {
            return;
        };
        if w.exited.is_some() {
            self.hide_widget();
            return;
        }
        let bytes = crate::pane::key_to_bytes(key, w.pane.app_cursor_mode());
        if !bytes.is_empty() {
            let _ = w.pane.write(&bytes);
            w.pane.scroll_to_bottom();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::tests::{BODY, open_app, screen, wait_for};
    use crossterm::event::{KeyCode, KeyModifiers};

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    fn with_widget(app: &mut App, name: &str, cfg: WidgetConfig) {
        app.settings.widget.insert(name.into(), cfg);
    }

    fn global(script: &str) -> WidgetConfig {
        WidgetConfig {
            command: sh(script),
            scope: WidgetScope::Global,
            ..Default::default()
        }
    }

    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    #[test]
    fn a_widget_shows_takes_keys_hides_and_comes_back_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = open_app(dir.path());
        with_widget(&mut app, "w", global("echo widget-ready; exec cat"));

        app.toggle_widgets(BODY); // the only widget: shown directly
        assert_eq!(app.widgets.shown, Some(("w".into(), None)));
        let w = app.widgets.shown().unwrap();
        wait_for(&w.pane, |s| s.contains("widget-ready"));

        // Keys go to the widget's program, not the focused pane.
        app.widget_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE), BODY);
        let text = wait_for(&app.widgets.shown().unwrap().pane, |s| s.contains('z'));
        assert!(text.contains("widget-ready\nz"), "{text}");

        // The Widgets shortcut hides it; it keeps running.
        app.widget_key(alt('e'), BODY);
        assert!(app.widgets.shown.is_none());
        assert_eq!(app.widgets.instances.len(), 1);

        // Summoned again: the same program, screen intact.
        app.show_widget("w", BODY).unwrap();
        assert_eq!(app.widgets.instances.len(), 1);
        assert!(screen(&app.widgets.shown().unwrap().pane).contains("widget-ready\nz"));
    }

    #[test]
    fn the_overlay_draws_over_the_ui_with_its_title_and_hide_key() {
        use ratatui::{Terminal, backend::TestBackend};
        let dir = tempfile::tempdir().unwrap();
        let mut app = open_app(dir.path());
        with_widget(
            &mut app,
            "prs",
            WidgetConfig {
                title: Some("Pull requests".into()),
                ..global("echo listing-prs; exec cat")
            },
        );
        app.show_widget("prs", BODY).unwrap();
        wait_for(&app.widgets.shown().unwrap().pane, |s| {
            s.contains("listing-prs")
        });

        let mut term = Terminal::new(TestBackend::new(BODY.width, BODY.height + 1)).unwrap();
        term.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer();
        let row =
            |y: u16| -> String { (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect() };
        let rect = overlay_rect(BODY);
        assert!(row(rect.y).contains(" Pull requests "), "{}", row(rect.y));
        assert!(row(rect.y).contains("Alt+e hide"));
        assert!(row(rect.y + 1).contains("listing-prs"));
    }

    #[test]
    fn a_project_widget_runs_per_project_in_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = open_app(dir.path());
        with_widget(
            &mut app,
            "w",
            WidgetConfig {
                command: sh("echo \"in=$WRK_PROJECT as=$WRK_WIDGET\"; pwd; exec cat"),
                ..Default::default()
            },
        );
        app.show_widget("w", BODY).unwrap();
        assert_eq!(app.widgets.shown, Some(("w".into(), Some("p".into()))));
        let text = wait_for(&app.widgets.shown().unwrap().pane, |s| s.contains("in=p"));
        assert!(text.contains("in=p as=w"), "{text}");
        let dir_name = dir.path().file_name().unwrap().to_string_lossy();
        assert!(
            text.contains(dir_name.as_ref()),
            "runs in the project dir: {text}"
        );
    }

    #[test]
    fn a_failure_stays_up_to_be_read_and_a_clean_exit_just_closes() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = open_app(dir.path());
        with_widget(&mut app, "bad", global("echo boom; exit 3"));
        with_widget(&mut app, "done", global("exit 0"));

        let reap = |app: &mut App, until: &dyn Fn(&App) -> bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !until(app) && std::time::Instant::now() < deadline {
                app.tick_widgets(BODY);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };

        app.show_widget("bad", BODY).unwrap();
        reap(&mut app, &|a| {
            a.widgets.shown().is_some_and(|w| w.exited.is_some())
        });
        let w = app.widgets.shown().unwrap();
        assert_eq!(w.exited.as_deref(), Some("exited with status 3"));
        assert!(
            screen(&w.pane).contains("boom"),
            "its output stays readable"
        );
        // Any key closes it, and the failed run is dropped.
        app.widget_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), BODY);
        assert!(app.widgets.shown.is_none());
        assert!(app.widgets.instances.is_empty());

        app.show_widget("done", BODY).unwrap();
        reap(&mut app, &|a| a.widgets.instances.is_empty());
        assert!(
            app.widgets.shown.is_none(),
            "a clean exit closes the overlay"
        );
        assert!(app.error.is_none());

        // A failure in the background is reported instead.
        app.show_widget("bad", BODY).unwrap();
        app.hide_widget();
        reap(&mut app, &|a| a.widgets.instances.is_empty());
        assert!(
            app.error
                .as_deref()
                .unwrap()
                .contains("widget 'bad' exited with status 3")
        );
    }

    #[test]
    fn the_widgets_key_offers_a_picker_when_there_are_several() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = open_app(dir.path());
        app.toggle_widgets(BODY);
        assert!(
            app.info
                .as_deref()
                .unwrap()
                .contains("no widgets configured")
        );

        with_widget(&mut app, "a", global("exec cat"));
        with_widget(
            &mut app,
            "b",
            WidgetConfig {
                title: Some("Bee".into()),
                ..global("exec cat")
            },
        );
        app.toggle_widgets(BODY);
        let Some(ModalState::WidgetPicker(m)) = &app.modal else {
            panic!("expected the widget picker");
        };
        assert_eq!(m.all, ["a", "Bee  (b)"]);
        assert!(
            app.widgets.instances.is_empty(),
            "nothing starts until picked"
        );
    }

    #[test]
    fn a_widgets_own_key_summons_it_and_switches_between_widgets() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = open_app(dir.path());
        with_widget(
            &mut app,
            "a",
            WidgetConfig {
                key: Some("Alt+r".into()),
                ..global("exec cat")
            },
        );
        with_widget(
            &mut app,
            "b",
            WidgetConfig {
                key: Some("Alt+y".into()),
                ..global("exec cat")
            },
        );
        let (keys, _) = summon_keys(&app.settings.widget, &app.keymap);
        app.widget_keys = keys;

        app.show_widget("a", BODY).unwrap();
        app.widget_key(alt('y'), BODY);
        assert_eq!(app.widgets.shown.as_ref().unwrap().0, "b", "switches");
        app.widget_key(alt('y'), BODY);
        assert!(app.widgets.shown.is_none(), "its own key hides it");
        assert_eq!(app.widgets.instances.len(), 2);
    }

    #[test]
    fn the_overlay_is_centered_and_keeps_a_usable_minimum() {
        let body = Rect::new(0, 0, 100, 40);
        let r = overlay_rect(body);
        assert_eq!((r.width, r.height), (80, 32));
        assert_eq!((r.x, r.y), (10, 4));
        // A small screen keeps 40×10 rather than shrinking to 4/5.
        let r = overlay_rect(Rect::new(0, 0, 44, 11));
        assert_eq!((r.width, r.height), (40, 10));
        // …but never more than the screen.
        let r = overlay_rect(Rect::new(0, 0, 30, 6));
        assert_eq!((r.width, r.height), (30, 6));
    }

    #[test]
    fn summon_keys_warn_on_bad_or_taken_keys() {
        let (keymap, _) =
            crate::keymap::KeyMap::build(&crate::settings::GlobalKeysConfig::default());
        let mut widgets = std::collections::BTreeMap::new();
        let w = |key: &str| WidgetConfig {
            command: vec!["true".into()],
            key: Some(key.into()),
            ..Default::default()
        };
        widgets.insert("prs".to_string(), w("Alt+r"));
        widgets.insert("bad".to_string(), w("Hyper+?"));
        widgets.insert("taken".to_string(), w("Alt+q"));
        let (keys, warnings) = summon_keys(&widgets, &keymap);
        assert_eq!(keys.len(), 1);
        assert_eq!(
            keys.get(&crate::keymap::parse_key("Alt+r").unwrap()),
            Some(&"prs".to_string())
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }
}
