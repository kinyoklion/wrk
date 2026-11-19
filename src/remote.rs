//! Projects that session plugins list themselves (the `projects` capability),
//! each plugin's shown as its own group in the sidebar beneath the local
//! projects from `projects.toml`.
//!
//! A listed project lives only in memory. It is rebuilt from every listing
//! as an ordinary plugin-backed [`Project`] (so the plugin attach paths serve
//! it unchanged), tagged with [`RemoteMeta`], keyed `"<plugin>/<id>"` so it
//! can't collide with a local project or another plugin's, and never written
//! to disk. Its tabs mirror the listing's sessions.
//!
//! wrk asks every configured plugin for a listing at startup (which launches
//! it: that's how wrk learns whether it lists projects at all), then every
//! [`REFRESH_EVERY`], after a create, and on `r`. A plugin that says it lists
//! none is not asked again this run.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use wrk_plugin::proto::{AgentStatus, RemoteProject};

use crate::plugin::Listing;
use crate::settings::HarnessKind;
use crate::store::{Project, RemoteMeta, SessionRef};

/// How often each listing plugin is asked again.
pub const REFRESH_EVERY: Duration = Duration::from_secs(20);
/// How long a just-created project or session is looked for in listings.
const OPEN_WAIT: Duration = Duration::from_secs(10 * 60);

/// The unique project key (`Project.name`) of plugin `plugin`'s project `id`.
pub fn key(plugin: &str, id: &str) -> String {
    format!("{plugin}/{id}")
}

/// One plugin's sidebar group.
#[derive(Debug, Clone)]
pub struct RemoteGroup {
    pub plugin: String,
    /// The heading: the listing's `group`, else the plugin's name.
    pub label: String,
    pub projects: Vec<Project>,
    /// Why the latest listing failed; the projects from the last good one stay.
    pub error: Option<String>,
    /// The plugin advertises `create`.
    pub can_create: bool,
    /// The plugin advertises `remove`.
    pub can_remove: bool,
    /// A listing has arrived at least once.
    pub listed: bool,
}

/// A project (and maybe a tab) to open as soon as a listing includes it: what
/// a create just made.
#[derive(Debug, Clone)]
pub struct PendingOpen {
    pub project: String,
    pub session: Option<String>,
    pub until: Instant,
}

#[derive(Debug, Default)]
pub struct Remote {
    /// In the order the plugins first answered.
    pub groups: Vec<RemoteGroup>,
    in_flight: HashSet<String>,
    last_asked: HashMap<String, Instant>,
    /// Plugins that answered that they list no projects.
    not_listing: HashSet<String>,
    pub pending_open: Option<PendingOpen>,
}

impl Remote {
    /// The plugins (of `configured`) to ask for a listing now; they're marked
    /// in flight, so a slow plugin is never asked twice at once.
    pub fn due(&mut self, configured: &[String], now: Instant) -> Vec<String> {
        let due: Vec<String> = configured
            .iter()
            .filter(|p| !self.not_listing.contains(*p) && !self.in_flight.contains(*p))
            .filter(|p| {
                self.last_asked
                    .get(*p)
                    .is_none_or(|at| now.duration_since(*at) >= REFRESH_EVERY)
            })
            .cloned()
            .collect();
        for plugin in &due {
            self.in_flight.insert(plugin.clone());
            self.last_asked.insert(plugin.clone(), now);
        }
        due
    }

    /// Ask `plugin` again on the next [`due`](Self::due).
    pub fn refresh_soon(&mut self, plugin: &str) {
        self.last_asked.remove(plugin);
    }

    /// Ask every listing plugin again on the next [`due`](Self::due).
    pub fn refresh_all(&mut self) {
        self.last_asked.clear();
    }

    /// Fold in a listing (or its failure). Returns the keys of projects that
    /// were listed before and aren't any more.
    pub fn apply(
        &mut self,
        plugin: &str,
        result: Result<Option<Listing>, String>,
        home: &Path,
    ) -> Vec<String> {
        self.in_flight.remove(plugin);
        let position = self.groups.iter().position(|g| g.plugin == plugin);
        match result {
            Ok(None) => {
                self.not_listing.insert(plugin.to_string());
                match position {
                    Some(i) => self
                        .groups
                        .remove(i)
                        .projects
                        .into_iter()
                        .map(|p| p.name)
                        .collect(),
                    None => Vec::new(),
                }
            }
            Err(error) => {
                let group = match position {
                    Some(i) => &mut self.groups[i],
                    None => {
                        self.groups.push(RemoteGroup {
                            plugin: plugin.to_string(),
                            label: plugin.to_string(),
                            projects: Vec::new(),
                            error: None,
                            can_create: false,
                            can_remove: false,
                            listed: false,
                        });
                        self.groups.last_mut().expect("just pushed")
                    }
                };
                group.error = Some(error);
                Vec::new()
            }
            Ok(Some(listing)) => {
                let old: Vec<Project> = position
                    .map(|i| std::mem::take(&mut self.groups[i].projects))
                    .unwrap_or_default();
                let projects: Vec<Project> = listing
                    .projects
                    .projects
                    .iter()
                    .map(|rp| {
                        let key = key(plugin, &rp.id);
                        synthesize(plugin, rp, home, old.iter().find(|p| p.name == key))
                    })
                    .collect();
                let vanished = old
                    .iter()
                    .filter(|o| !projects.iter().any(|p| p.name == o.name))
                    .map(|o| o.name.clone())
                    .collect();
                let group = RemoteGroup {
                    plugin: plugin.to_string(),
                    label: listing
                        .projects
                        .group
                        .filter(|g| !g.trim().is_empty())
                        .unwrap_or_else(|| plugin.to_string()),
                    projects,
                    error: None,
                    can_create: listing.can_create,
                    can_remove: listing.can_remove,
                    listed: true,
                };
                match position {
                    Some(i) => self.groups[i] = group,
                    None => self.groups.push(group),
                }
                vanished
            }
        }
    }

    pub fn find(&self, key: &str) -> Option<&Project> {
        self.groups
            .iter()
            .flat_map(|g| &g.projects)
            .find(|p| p.name == key)
    }

    pub fn find_mut(&mut self, key: &str) -> Option<&mut Project> {
        self.groups
            .iter_mut()
            .flat_map(|g| &mut g.projects)
            .find(|p| p.name == key)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.find(key).is_some()
    }

    pub fn group(&self, plugin: &str) -> Option<&RemoteGroup> {
        self.groups.iter().find(|g| g.plugin == plugin)
    }

    /// Open `project` (and its tab `session`) once a listing includes it.
    pub fn open_when_listed(&mut self, project: String, session: Option<String>, now: Instant) {
        self.pending_open = Some(PendingOpen {
            project,
            session,
            until: now + OPEN_WAIT,
        });
    }

    /// The pending open, once its project (and session, when one was named)
    /// is listed: the project key and the tab index to show. Forgets it then,
    /// or once it has waited too long.
    pub fn take_ready_open(&mut self, now: Instant) -> Option<(String, Option<usize>)> {
        let pending = self.pending_open.as_ref()?;
        if now >= pending.until {
            self.pending_open = None;
            return None;
        }
        let project = self.find(&pending.project)?;
        let tab = match &pending.session {
            None => None,
            Some(id) => Some(
                project
                    .claude_sessions
                    .iter()
                    .position(|s| s.session_id.as_deref() == Some(id.as_str()))?,
            ),
        };
        let key = project.name.clone();
        self.pending_open = None;
        Some((key, tab))
    }
}

/// A listed project as an ordinary plugin-backed [`Project`]: its sessions
/// become its tab list, its `config` the plugin config sent back on every
/// request, and its cwd the home directory (the pane commands run locally).
/// What the person changed in memory (layout, passthrough) carries over from
/// `previous`, the same project in the last listing.
pub fn synthesize(
    plugin: &str,
    listed: &RemoteProject,
    home: &Path,
    previous: Option<&Project>,
) -> Project {
    Project {
        name: key(plugin, &listed.id),
        path: home.to_path_buf(),
        tags: Vec::new(),
        layout_mode: previous.and_then(|p| p.layout_mode),
        shell_passthrough: previous.and_then(|p| p.shell_passthrough),
        claude_sessions: listed
            .sessions
            .iter()
            .map(|s| SessionRef {
                name: s
                    .name
                    .clone()
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| s.id.clone()),
                session_id: Some(s.id.clone()),
                harness: HarnessKind::default(),
            })
            .collect(),
        session_names: Default::default(),
        plugin: Some(plugin.to_string()),
        plugin_config: listed.config.clone(),
        remote: Some(RemoteMeta {
            plugin: plugin.to_string(),
            id: listed.id.clone(),
            label: if listed.name.is_empty() {
                listed.id.clone()
            } else {
                listed.name.clone()
            },
            state: listed.state.clone().filter(|s| !s.is_empty()),
            sessions: listed.sessions.clone(),
        }),
    }
}

/// The listing's status for `project`'s session `session_id`.
pub fn listed_status(project: &Project, session_id: Option<&str>) -> Option<AgentStatus> {
    let id = session_id?;
    project
        .remote
        .as_ref()?
        .sessions
        .iter()
        .find(|s| s.id == id)?
        .status
}

/// The user's home directory, the cwd for a listed project's pane commands.
pub fn home_dir() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use wrk_plugin::proto::{PluginSession, ProjectsResponse};

    fn session(id: &str, name: &str, status: Option<AgentStatus>) -> PluginSession {
        PluginSession {
            id: id.into(),
            name: Some(name.into()),
            state: Default::default(),
            status,
        }
    }

    fn listing(projects: Vec<RemoteProject>) -> Result<Option<Listing>, String> {
        Ok(Some(Listing {
            projects: ProjectsResponse {
                group: Some("boxes".into()),
                projects,
            },
            can_create: true,
            can_remove: true,
        }))
    }

    fn sdk_research(sessions: Vec<PluginSession>) -> RemoteProject {
        RemoteProject {
            id: "i-1".into(),
            name: "dev-box".into(),
            state: Some("running".into()),
            config: BTreeMap::from([("box".to_string(), "i-1".to_string())]),
            sessions,
        }
    }

    #[test]
    fn a_listing_becomes_keyed_in_memory_plugin_projects() {
        let mut remote = Remote::default();
        let home = Path::new("/home/me");
        let gone = remote.apply(
            "boxes",
            listing(vec![sdk_research(vec![
                session("p1/main", "web-main", Some(AgentStatus::Busy)),
                session("p1/fix", "web-fix", None),
            ])]),
            home,
        );
        assert!(gone.is_empty());
        let group = remote.group("boxes").unwrap();
        assert_eq!(
            (group.label.as_str(), group.can_create, group.listed),
            ("boxes", true, true)
        );
        let p = remote.find("boxes/i-1").expect("keyed by plugin and id");
        assert_eq!(p.display_name(), "dev-box");
        assert_eq!(p.plugin.as_deref(), Some("boxes"));
        assert_eq!(p.plugin_config["box"], "i-1");
        assert_eq!(p.path, home);
        let tabs: Vec<(&str, Option<&str>)> = p
            .claude_sessions
            .iter()
            .map(|s| (s.name.as_str(), s.session_id.as_deref()))
            .collect();
        assert_eq!(
            tabs,
            [("web-main", Some("p1/main")), ("web-fix", Some("p1/fix"))]
        );
        assert_eq!(listed_status(p, Some("p1/main")), Some(AgentStatus::Busy));
        assert_eq!(listed_status(p, Some("p1/fix")), None);
    }

    #[test]
    fn relisting_keeps_in_memory_choices_and_reports_what_vanished() {
        let mut remote = Remote::default();
        let home = Path::new("/h");
        remote.apply("boxes", listing(vec![sdk_research(vec![])]), home);
        remote.find_mut("boxes/i-1").unwrap().layout_mode = Some(crate::store::LayoutMode::Split);
        remote.apply("boxes", listing(vec![sdk_research(vec![])]), home);
        assert_eq!(
            remote.find("boxes/i-1").unwrap().layout_mode,
            Some(crate::store::LayoutMode::Split)
        );
        assert_eq!(remote.apply("boxes", listing(vec![]), home), ["boxes/i-1"]);
        assert!(!remote.contains("boxes/i-1"));
    }

    #[test]
    fn a_failed_listing_keeps_the_last_projects_and_says_why() {
        let mut remote = Remote::default();
        let home = Path::new("/h");
        remote.apply("boxes", listing(vec![sdk_research(vec![])]), home);
        remote.apply("boxes", Err("not signed in".into()), home);
        let group = remote.group("boxes").unwrap();
        assert_eq!(group.error.as_deref(), Some("not signed in"));
        assert_eq!(group.projects.len(), 1);
        // A plugin that fails before it ever listed still gets a group, to say so.
        remote.apply("other", Err("starting the plugin failed".into()), home);
        assert!(remote.group("other").unwrap().projects.is_empty());
    }

    #[test]
    fn listings_are_due_at_startup_then_periodically_never_twice_at_once() {
        let mut remote = Remote::default();
        let plugins = vec!["boxes".to_string(), "claude-ext".to_string()];
        let t0 = Instant::now();
        assert_eq!(remote.due(&plugins, t0), plugins);
        assert!(
            remote.due(&plugins, t0 + REFRESH_EVERY * 2).is_empty(),
            "in flight"
        );
        remote.apply("boxes", listing(vec![]), Path::new("/h"));
        remote.apply("claude-ext", Ok(None), Path::new("/h"));
        assert!(remote.due(&plugins, t0 + Duration::from_secs(1)).is_empty());
        // claude-ext lists nothing: never asked again.
        assert_eq!(remote.due(&plugins, t0 + REFRESH_EVERY), ["boxes"]);
        remote.apply("boxes", listing(vec![]), Path::new("/h"));
        remote.refresh_soon("boxes");
        assert_eq!(remote.due(&plugins, t0 + REFRESH_EVERY), ["boxes"]);
    }

    #[test]
    fn a_created_session_is_opened_once_a_listing_has_it() {
        let mut remote = Remote::default();
        let now = Instant::now();
        let home = Path::new("/h");
        remote.open_when_listed("boxes/i-1".into(), Some("p2/main".into()), now);
        assert_eq!(remote.take_ready_open(now), None, "not listed yet");
        remote.apply(
            "boxes",
            listing(vec![sdk_research(vec![session("p1/main", "a", None)])]),
            home,
        );
        assert_eq!(
            remote.take_ready_open(now),
            None,
            "the project, but not the session yet"
        );
        remote.apply(
            "boxes",
            listing(vec![sdk_research(vec![
                session("p1/main", "a", None),
                session("p2/main", "b", None),
            ])]),
            home,
        );
        assert_eq!(
            remote.take_ready_open(now),
            Some(("boxes/i-1".into(), Some(1)))
        );
        assert!(remote.pending_open.is_none());

        remote.open_when_listed("boxes/nope".into(), None, now);
        assert_eq!(remote.take_ready_open(now + OPEN_WAIT), None);
        assert!(remote.pending_open.is_none(), "gave up");
    }
}
