//! The projects sidebar: a tree of projects, each with its sessions nested
//! (indented) beneath it, plus a fuzzy filter over project and session names.
//! Projects come in groups: the local ones from `projects.toml`, then one
//! group per session plugin that lists its own (`src/remote.rs`), each under
//! a heading row. The local group's heading only shows once a plugin group
//! exists, so a plugin-free sidebar looks as it always has.
//!
//! ```text
//! local
//! wrk *
//!   ● main
//!   · handle issues
//!   ▤ README.md
//! notes
//!     ideas            (not loaded: no indicator, dimmed)
//! boxes
//! dev-box
//!   ● web-main
//! ```
//!
//! Rows are rebuilt from the groups and the live sessions every frame
//! ([`ProjectSidebar::rebuild`]); the selection is tracked by [`RowKey`] so it
//! stays on the same project/session while rows come and go around it.

use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, List, ListItem, ListState, Paragraph, StatefulWidget, Widget,
};

use super::{cell_width, truncate_to_width};
use crate::store::Project;

/// One section of the sidebar: the local projects, or one plugin's.
pub struct SidebarGroup<'a> {
    /// `""` for the local group, else the plugin's name.
    pub id: String,
    /// The heading row's text; `None` draws no heading.
    pub label: Option<String>,
    /// Shown dimmed after the heading ("loading…", why a listing failed).
    pub note: Option<String>,
    pub projects: Vec<&'a Project>,
}

impl SidebarGroup<'_> {
    /// The id of the local group.
    pub const LOCAL: &'static str = "";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectStatus {
    /// No live session for this project.
    None,
    /// Claude pane is producing output (recent bytes seen) or has just
    /// received a UserPromptSubmit event.
    Busy,
    /// Claude has finished its response and is waiting for input
    /// (Stop hook), or has been quiet long enough for the heuristic.
    Waiting,
    /// Claude has signalled a Notification (e.g. permission prompt).
    Attention,
}

/// What a session row shows in its indicator column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// An agent tab, with its activity status.
    Agent(ProjectStatus),
    /// A markdown viewer tab.
    Markdown,
}

/// One of a project's sessions (primary-pane tabs), as the sidebar lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub name: String,
    pub kind: SessionKind,
    /// The tab currently on screen (the active project's shown tab).
    pub active: bool,
}

/// What the sidebar lists under one project.
#[derive(Debug, Clone, Default)]
pub struct ProjectView {
    /// The project has a live session in memory.
    pub loaded: bool,
    /// Its tabs in order: the live tabs when loaded, else the persisted session
    /// list (which becomes the tab list, in the same order, on open). A session
    /// row's index here is therefore the tab it opens.
    pub sessions: Vec<SessionInfo>,
}

/// Identity of a sidebar row that survives rebuilds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKey {
    /// A group's heading row, by group id.
    Group(String),
    /// A project header row, by project key (`Project.name`).
    Project(String),
    /// A session row: project key + tab index.
    Session(String, usize),
}

#[derive(Debug, Clone)]
pub struct SidebarRow {
    /// The id of the row's group ([`SidebarGroup::id`]).
    pub group: String,
    /// The row's project key (`Project.name`); empty on a group heading.
    pub project_name: String,
    /// What a heading or project row shows: the group's heading or the
    /// project's display name.
    pub label: String,
    /// Shown dimmed after the label: a group's note, a listed project's state.
    pub note: Option<String>,
    /// A group heading rather than a project or session.
    pub heading: bool,
    /// The project has a live session in memory.
    pub loaded: bool,
    /// `Some((tab index, session))` for a session row; `None` for the header.
    pub session: Option<(usize, SessionInfo)>,
    /// Whether the row itself matched the filter (always true unfiltered). A
    /// header listed only because some of its sessions matched is `false`.
    pub matched: bool,
}

impl SidebarRow {
    pub fn key(&self) -> RowKey {
        if self.heading {
            return RowKey::Group(self.group.clone());
        }
        match &self.session {
            None => RowKey::Project(self.project_name.clone()),
            Some((tab, _)) => RowKey::Session(self.project_name.clone(), *tab),
        }
    }
}

/// Indent for session rows, so they read as grouped under their project.
const INDENT: &str = "  ";

pub struct ProjectSidebar {
    pub state: ListState,
    /// The filter query while filtering (`/`); `None` otherwise.
    pub filter: Option<String>,
    pub focused: bool,
    /// The rows as last built, in display order.
    pub rows: Vec<SidebarRow>,
    /// Name of the currently active project (rendered with a marker).
    pub active: Option<String>,
    /// The query `rows` were last built with. A changed, non-empty query moves
    /// the selection to the best match instead of keeping it in place.
    built_query: String,
}

impl Default for ProjectSidebar {
    fn default() -> Self {
        Self {
            state: ListState::default(),
            filter: None,
            focused: true,
            rows: Vec::new(),
            active: None,
            built_query: String::new(),
        }
    }
}

impl ProjectSidebar {
    /// Rebuild the rows from `groups`, asking `view_for` what each project
    /// lists. Keeps the selection on the same row (by [`RowKey`]) where it
    /// still exists — falling back to its project's header, then the nearest
    /// index — except when the filter query just changed, which selects the
    /// first row that actually matched.
    pub fn rebuild<F>(&mut self, groups: &[SidebarGroup<'_>], view_for: F)
    where
        F: Fn(&Project) -> ProjectView,
    {
        let old_key = self.selected_row().map(SidebarRow::key);
        let old_idx = self.state.selected();
        let query = self.filter.clone().unwrap_or_default();
        let query_changed = query != self.built_query;
        self.rows = build_rows(groups, &view_for, &query);
        self.built_query = query;

        if self.rows.is_empty() {
            self.state.select(None);
            return;
        }
        let next = if query_changed && !self.built_query.is_empty() {
            self.rows.iter().position(|r| r.matched)
        } else {
            old_key.and_then(|key| {
                self.index_of(&key).or_else(|| match key {
                    RowKey::Session(project, _) => self.index_of(&RowKey::Project(project)),
                    RowKey::Project(_) | RowKey::Group(_) => None,
                })
            })
        };
        let fallback = old_idx.unwrap_or(0).min(self.rows.len() - 1);
        self.state.select(Some(next.unwrap_or(fallback)));
    }

    fn index_of(&self, key: &RowKey) -> Option<usize> {
        self.rows.iter().position(|r| &r.key() == key)
    }

    /// Move the selection to `key`'s row; returns whether it is listed.
    pub fn select_key(&mut self, key: &RowKey) -> bool {
        match self.index_of(key) {
            Some(i) => {
                self.state.select(Some(i));
                true
            }
            None => false,
        }
    }

    pub fn selected_row(&self) -> Option<&SidebarRow> {
        self.state.selected().and_then(|i| self.rows.get(i))
    }

    /// The selected row's project key (header or session row; not a heading).
    pub fn selected_project(&self) -> Option<&str> {
        self.selected_row()
            .filter(|r| !r.heading)
            .map(|r| r.project_name.as_str())
    }

    /// The group of the selected row, whatever its kind.
    pub fn selected_group(&self) -> Option<&str> {
        self.selected_row().map(|r| r.group.as_str())
    }

    /// What opening the selection opens: the project's key, plus the tab to
    /// show when a session row is selected. `None` on a group heading.
    pub fn selected_target(&self) -> Option<(String, Option<usize>)> {
        self.selected_row().filter(|r| !r.heading).map(|r| {
            (
                r.project_name.clone(),
                r.session.as_ref().map(|(tab, _)| *tab),
            )
        })
    }

    pub fn select_next(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let i = self
            .state
            .selected()
            .map_or(0, |i| (i + 1).min(self.rows.len() - 1));
        self.state.select(Some(i));
    }

    pub fn select_prev(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let i = self.state.selected().map_or(0, |i| i.saturating_sub(1));
        self.state.select(Some(i));
    }

    /// The row index under screen row `y` for a sidebar drawn in `area`.
    pub fn row_at(&self, area: Rect, y: u16) -> Option<usize> {
        let list = self.list_area(area);
        if y < list.y || y >= list.y + list.height {
            return None;
        }
        let idx = (y - list.y) as usize + self.state.offset();
        (idx < self.rows.len()).then_some(idx)
    }

    /// Where the rows go inside the bordered `area`: below the filter input
    /// line while filtering.
    fn list_area(&self, area: Rect) -> Rect {
        let inner = Block::default().borders(Borders::ALL).inner(area);
        if self.filter.is_some() && inner.height > 0 {
            Rect {
                y: inner.y + 1,
                height: inner.height - 1,
                ..inner
            }
        } else {
            inner
        }
    }

    /// Draw the sidebar. Returns where the cursor belongs while the filter
    /// input is open (the caller places it when the sidebar has focus).
    pub fn render(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        theme: &crate::settings::Theme,
    ) -> Option<Position> {
        let border_style = Style::default().fg(if self.focused {
            theme.border_focused
        } else {
            theme.border_unfocused
        });
        let block = Block::default()
            .title(" projects ")
            .borders(Borders::ALL)
            .border_style(border_style);
        let inner = block.inner(area);
        block.render(area, buf);

        let mut cursor = None;
        if let Some(query) = &self.filter
            && inner.height > 0
        {
            let line = Rect { height: 1, ..inner };
            const PROMPT: &str = "/ ";
            // Keep the end of a long query in view, leaving a cell for the cursor.
            let room = (line.width as usize).saturating_sub(cell_width(PROMPT) + 1);
            let shown = tail_to_width(query, room);
            let x = line.x + (cell_width(PROMPT) + cell_width(&shown)) as u16;
            Paragraph::new(Line::from(vec![
                Span::styled(PROMPT, Style::default().fg(theme.accent)),
                Span::raw(shown),
            ]))
            .render(line, buf);
            cursor = Some(Position::new(x.min(line.right().saturating_sub(1)), line.y));
        }

        let list_area = self.list_area(area);
        if self.rows.is_empty() {
            if self.filter.as_deref().is_some_and(|q| !q.is_empty()) {
                Paragraph::new("no matches")
                    .style(Style::default().fg(theme.hint))
                    .render(list_area, buf);
            }
            return cursor;
        }

        let width = list_area.width as usize;
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|row| ListItem::new(self.row_line(row, width, theme)))
            .collect();
        let list = List::new(items).highlight_style(
            Style::default()
                .fg(theme.accent_fg)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD),
        );
        StatefulWidget::render(list, list_area, buf, &mut self.state);
        cursor
    }

    fn row_line(
        &self,
        row: &SidebarRow,
        width: usize,
        theme: &crate::settings::Theme,
    ) -> Line<'static> {
        if row.heading {
            // A group heading: its name, set apart in the accent color.
            let note = row
                .note
                .as_deref()
                .map(|n| format!(" {n}"))
                .unwrap_or_default();
            let name = truncate_to_width(&row.label, width);
            let room = width.saturating_sub(cell_width(&name));
            return Line::from(vec![
                Span::styled(
                    name,
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                ),
                Span::styled(
                    truncate_to_width(&note, room),
                    Style::default().fg(theme.hint),
                ),
            ]);
        }
        let Some((_, session)) = &row.session else {
            // Project header. `*` marks a project with a live session: accent on
            // the active (shown) project, dim on ones loaded in the background.
            let active = self.active.as_deref() == Some(row.project_name.as_str());
            let marker = if active {
                Span::styled(" *", Style::default().fg(theme.accent))
            } else if row.loaded {
                Span::styled(" *", Style::default().fg(theme.hint))
            } else {
                Span::raw("")
            };
            let mut style = Style::default().add_modifier(Modifier::BOLD);
            if !row.matched {
                // Listed only as context for matching sessions.
                style = style.fg(theme.hint);
            }
            let note = row
                .note
                .as_deref()
                .map(|n| format!(" {n}"))
                .unwrap_or_default();
            let name = truncate_to_width(&row.label, width.saturating_sub(marker.width()));
            let room = width.saturating_sub(cell_width(&name) + marker.width());
            return Line::from(vec![
                Span::styled(name, style),
                Span::styled(
                    truncate_to_width(&note, room),
                    Style::default().fg(theme.hint),
                ),
                marker,
            ]);
        };

        let (glyph, color) = match session.kind {
            SessionKind::Agent(ProjectStatus::Attention) => ("● ", theme.status_attention),
            SessionKind::Agent(ProjectStatus::Waiting) => ("● ", theme.status_waiting),
            SessionKind::Agent(ProjectStatus::Busy) => ("· ", theme.status_busy),
            SessionKind::Agent(ProjectStatus::None) => ("  ", theme.hint),
            SessionKind::Markdown => ("▤ ", theme.hint),
        };
        let name_style = if session.active {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else if row.loaded {
            Style::default()
        } else {
            // Persisted but not running: opening the project resumes it.
            Style::default().fg(theme.hint)
        };
        let room = width.saturating_sub(cell_width(INDENT) + cell_width(glyph));
        Line::from(vec![
            Span::raw(INDENT),
            Span::styled(glyph, Style::default().fg(color)),
            Span::styled(truncate_to_width(&session.name, room), name_style),
        ])
    }
}

/// Lay out the tree for `query` (empty = unfiltered). A project is listed when
/// its name or any of its sessions match; under it go the matching sessions.
/// A session is matched against `"<project> <session>"`, so a query naming the
/// project lists all its sessions and `"wrk issues"` narrows to one. Filtered
/// projects are ordered by their best match within their group (groups keep
/// their order); sessions keep their tab order. A heading is listed while its
/// group has rows to show, and always when unfiltered.
fn build_rows<F>(groups: &[SidebarGroup<'_>], view_for: &F, query: &str) -> Vec<SidebarRow>
where
    F: Fn(&Project) -> ProjectView,
{
    let mut rows = Vec::new();
    for group in groups {
        let projects = group_rows(group, view_for, query);
        if let Some(label) = &group.label
            && (query.is_empty() || !projects.is_empty())
        {
            rows.push(SidebarRow {
                group: group.id.clone(),
                project_name: String::new(),
                label: label.clone(),
                note: group.note.clone(),
                heading: true,
                loaded: false,
                session: None,
                // Context while filtering, never a match of its own.
                matched: query.is_empty(),
            });
        }
        rows.extend(projects);
    }
    rows
}

/// One group's project and session rows for `query`.
fn group_rows<F>(group: &SidebarGroup<'_>, view_for: &F, query: &str) -> Vec<SidebarRow>
where
    F: Fn(&Project) -> ProjectView,
{
    let pattern = (!query.is_empty())
        .then(|| Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart));
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buf = Vec::new();
    let mut score = |text: &str| -> Option<u32> {
        match &pattern {
            None => Some(0),
            Some(p) => p.score(Utf32Str::new(text, &mut buf), &mut matcher),
        }
    };

    let mut groups: Vec<(u32, Vec<SidebarRow>)> = Vec::new();
    for project in &group.projects {
        let view = view_for(project);
        let label = project.display_name();
        let header_score = score(label);
        let mut best = header_score;
        let mut sessions = Vec::new();
        for (tab, info) in view.sessions.into_iter().enumerate() {
            if let Some(s) = score(&format!("{label} {}", info.name)) {
                best = best.max(Some(s));
                sessions.push((tab, info));
            }
        }
        let Some(best) = best else {
            continue;
        };
        let note = project.remote.as_ref().and_then(|r| r.state.clone());
        let row = |session: Option<(usize, SessionInfo)>, matched: bool| SidebarRow {
            group: group.id.clone(),
            project_name: project.name.clone(),
            label: label.to_string(),
            note: note.clone(),
            heading: false,
            loaded: view.loaded,
            session,
            matched,
        };
        let mut group = vec![row(None, header_score.is_some())];
        group.extend(sessions.into_iter().map(|s| row(Some(s), true)));
        groups.push((best, group));
    }
    if pattern.is_some() {
        // Stable: equal scores keep store order.
        groups.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    }
    groups.into_iter().flat_map(|(_, rows)| rows).collect()
}

/// The end of `s` that fits in `max` cells, led by `…` when cut.
fn tail_to_width(s: &str, max: usize) -> String {
    if cell_width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut kept = Vec::new();
    let mut used = 0;
    for ch in s.chars().rev() {
        let mut tmp = [0u8; 4];
        let w = cell_width(ch.encode_utf8(&mut tmp));
        if used + w > max - 1 {
            break;
        }
        kept.push(ch);
        used += w;
    }
    std::iter::once('…').chain(kept.into_iter().rev()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn project(name: &str) -> Project {
        Project {
            name: name.to_string(),
            path: PathBuf::from("/tmp"),
            tags: vec![],
            layout_mode: None,
            shell_passthrough: None,
            claude_sessions: vec![],
            session_names: BTreeMap::new(),
            plugin: None,
            plugin_config: BTreeMap::new(),
            remote: None,
        }
    }

    /// The local group alone, as wrk builds it when no plugin lists projects.
    fn local(projects: &[Project]) -> Vec<SidebarGroup<'_>> {
        vec![SidebarGroup {
            id: SidebarGroup::LOCAL.into(),
            label: None,
            note: None,
            projects: projects.iter().collect(),
        }]
    }

    fn store(names: &[&str]) -> Vec<Project> {
        names.iter().map(|n| project(n)).collect()
    }

    fn remote(plugin: &str, id: &str, label: &str, state: Option<&str>) -> Project {
        Project {
            name: format!("{plugin}/{id}"),
            remote: Some(crate::store::RemoteMeta {
                plugin: plugin.into(),
                id: id.into(),
                label: label.into(),
                state: state.map(str::to_string),
                sessions: vec![],
            }),
            ..project(label)
        }
    }

    fn agent(name: &str) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            kind: SessionKind::Agent(ProjectStatus::Waiting),
            active: false,
        }
    }

    /// `wrk` has three sessions, `notes` one, `empty` none.
    fn view(p: &Project) -> ProjectView {
        let names: &[&str] = match p.name.as_str() {
            "wrk" => &["main", "handle issues", "cool feature"],
            "notes" => &["ideas"],
            _ => &[],
        };
        ProjectView {
            loaded: true,
            sessions: names.iter().map(|n| agent(n)).collect(),
        }
    }

    fn filtered(query: &str) -> ProjectSidebar {
        ProjectSidebar {
            filter: Some(query.to_string()),
            ..Default::default()
        }
    }

    fn keys(sb: &ProjectSidebar) -> Vec<RowKey> {
        sb.rows.iter().map(SidebarRow::key).collect()
    }

    fn session(p: &str, tab: usize) -> RowKey {
        RowKey::Session(p.to_string(), tab)
    }

    fn header(p: &str) -> RowKey {
        RowKey::Project(p.to_string())
    }

    #[test]
    fn unfiltered_nests_sessions_under_their_project() {
        let mut sb = ProjectSidebar::default();
        sb.rebuild(&local(&store(&["wrk", "notes", "empty"])), view);
        assert_eq!(
            keys(&sb),
            vec![
                header("wrk"),
                session("wrk", 0),
                session("wrk", 1),
                session("wrk", 2),
                header("notes"),
                session("notes", 0),
                header("empty"),
            ]
        );
        assert!(sb.rows.iter().all(|r| r.matched));
        assert_eq!(sb.state.selected(), Some(0));
    }

    #[test]
    fn filter_on_a_session_lists_its_project_as_context_and_selects_the_session() {
        let mut sb = filtered("issues");
        sb.rebuild(&local(&store(&["wrk", "notes"])), view);
        assert_eq!(keys(&sb), vec![header("wrk"), session("wrk", 1)]);
        assert!(!sb.rows[0].matched, "header is context only");
        assert_eq!(sb.selected_target(), Some(("wrk".into(), Some(1))));
    }

    #[test]
    fn filter_on_a_project_lists_all_its_sessions() {
        let mut sb = filtered("notes");
        sb.rebuild(&local(&store(&["wrk", "notes"])), view);
        assert_eq!(keys(&sb), vec![header("notes"), session("notes", 0)]);
        assert_eq!(sb.selected_target(), Some(("notes".into(), None)));
    }

    #[test]
    fn project_and_session_terms_narrow_together() {
        let mut sb = filtered("wrk cool");
        sb.rebuild(&local(&store(&["wrk", "notes"])), view);
        assert_eq!(keys(&sb), vec![header("wrk"), session("wrk", 2)]);
    }

    #[test]
    fn no_match_lists_nothing() {
        let mut sb = filtered("zzzz");
        sb.rebuild(&local(&store(&["wrk", "notes"])), view);
        assert!(sb.rows.is_empty());
        assert_eq!(sb.state.selected(), None);
        assert_eq!(sb.selected_target(), None);
    }

    #[test]
    fn selection_follows_its_row_when_rows_shift() {
        let mut sb = ProjectSidebar::default();
        let s = store(&["notes", "wrk"]);
        let s = local(&s);
        sb.rebuild(&s, view);
        assert!(sb.select_key(&session("wrk", 1)));
        // `notes` gains a session above the selection.
        sb.rebuild(&s, |p| {
            let mut v = view(p);
            if p.name == "notes" {
                v.sessions.push(agent("more"));
            }
            v
        });
        assert_eq!(
            sb.selected_row().map(SidebarRow::key),
            Some(session("wrk", 1))
        );
    }

    #[test]
    fn a_vanished_session_falls_back_to_its_project() {
        let mut sb = ProjectSidebar::default();
        let s = store(&["wrk", "notes"]);
        let s = local(&s);
        sb.rebuild(&s, view);
        assert!(sb.select_key(&session("wrk", 2)));
        sb.rebuild(&s, |p| {
            let mut v = view(p);
            v.sessions.truncate(1);
            v
        });
        assert_eq!(sb.selected_row().map(SidebarRow::key), Some(header("wrk")));
    }

    #[test]
    fn clearing_the_filter_keeps_the_selected_row() {
        let mut sb = ProjectSidebar::default();
        let s = store(&["wrk", "notes"]);
        let s = local(&s);
        sb.filter = Some("cool".into());
        sb.rebuild(&s, view);
        assert_eq!(
            sb.selected_row().map(SidebarRow::key),
            Some(session("wrk", 2))
        );
        sb.filter = None;
        sb.rebuild(&s, view);
        assert_eq!(
            sb.selected_row().map(SidebarRow::key),
            Some(session("wrk", 2))
        );
    }

    #[test]
    fn row_at_accounts_for_border_and_filter_line() {
        let mut sb = ProjectSidebar::default();
        sb.rebuild(&local(&store(&["wrk", "notes"])), view);
        let area = Rect::new(0, 0, 24, 20);
        assert_eq!(sb.row_at(area, 0), None, "top border");
        assert_eq!(sb.row_at(area, 1), Some(0));
        assert_eq!(sb.row_at(area, 3), Some(2));
        assert_eq!(sb.row_at(area, 7), None, "past the last row");
        sb.filter = Some(String::new());
        sb.rebuild(&local(&store(&["wrk", "notes"])), view);
        assert_eq!(sb.row_at(area, 1), None, "filter input line");
        assert_eq!(sb.row_at(area, 2), Some(0));
    }

    fn rendered(sb: &mut ProjectSidebar, area: Rect) -> Vec<String> {
        let mut buf = Buffer::empty(area);
        sb.render(area, &mut buf, &crate::settings::Theme::default());
        (area.y..area.bottom())
            .map(|y| {
                (area.x..area.right())
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn renders_sessions_inset_with_indicators() {
        let mut sb = ProjectSidebar::default();
        sb.rebuild(&local(&store(&["wrk"])), |p| {
            let mut v = view(p);
            v.sessions[1].kind = SessionKind::Agent(ProjectStatus::Busy);
            v.sessions[2].kind = SessionKind::Markdown;
            v
        });
        let lines = rendered(&mut sb, Rect::new(0, 0, 24, 6));
        assert_eq!(lines[1], "│wrk *                 │");
        assert_eq!(lines[2], "│  ● main              │");
        assert_eq!(lines[3], "│  · handle issues     │");
        assert_eq!(lines[4], "│  ▤ cool feature      │");
    }

    #[test]
    fn long_session_names_truncate_with_an_ellipsis() {
        let mut sb = ProjectSidebar::default();
        sb.rebuild(&local(&store(&["wrk"])), |_| ProjectView {
            loaded: false,
            sessions: vec![agent("implement my cool new feature")],
        });
        let lines = rendered(&mut sb, Rect::new(0, 0, 24, 4));
        assert_eq!(lines[2], "│  ● implement my cool…│");
    }

    #[test]
    fn plugin_groups_get_headings_and_the_local_group_one_too() {
        let locals = store(&["wrk"]);
        let boxes = [
            remote("boxes", "i-1", "dev-box", None),
            remote("boxes", "i-2", "scratch", Some("stopped")),
        ];
        let groups = vec![
            SidebarGroup {
                id: SidebarGroup::LOCAL.into(),
                label: Some("local".into()),
                note: None,
                projects: locals.iter().collect(),
            },
            SidebarGroup {
                id: "boxes".into(),
                label: Some("boxes".into()),
                note: None,
                projects: boxes.iter().collect(),
            },
        ];
        let mut sb = ProjectSidebar::default();
        sb.rebuild(&groups, |p| match p.display_name() {
            "dev-box" => ProjectView {
                loaded: false,
                sessions: vec![agent("web-main"), agent("web-fix")],
            },
            _ => view(p),
        });
        assert_eq!(
            keys(&sb),
            vec![
                RowKey::Group("".into()),
                header("wrk"),
                session("wrk", 0),
                session("wrk", 1),
                session("wrk", 2),
                RowKey::Group("boxes".into()),
                header("boxes/i-1"),
                session("boxes/i-1", 0),
                session("boxes/i-1", 1),
                header("boxes/i-2"),
            ]
        );
        // A heading opens nothing; a listed project opens by its key.
        assert_eq!(sb.selected_target(), None);
        assert_eq!(sb.selected_group(), Some(""));
        assert!(sb.select_key(&session("boxes/i-1", 1)));
        assert_eq!(sb.selected_target(), Some(("boxes/i-1".into(), Some(1))));
        assert_eq!(sb.selected_project(), Some("boxes/i-1"));
        assert_eq!(sb.selected_group(), Some("boxes"));

        let lines = rendered(&mut sb, Rect::new(0, 0, 24, 12));
        assert_eq!(lines[6], "│boxes                 │");
        assert_eq!(lines[7], "│dev-box               │");
        assert_eq!(lines[8], "│  ● web-main          │");
        assert_eq!(lines[10], "│scratch stopped *     │");

        // Filtering keeps groups apart and lists a heading only above matches.
        let mut sb = filtered("fix");
        sb.rebuild(&groups, |p| match p.display_name() {
            "dev-box" => ProjectView {
                loaded: false,
                sessions: vec![agent("web-main"), agent("web-fix")],
            },
            _ => ProjectView::default(),
        });
        assert_eq!(
            keys(&sb),
            vec![
                RowKey::Group("boxes".into()),
                header("boxes/i-1"),
                session("boxes/i-1", 1)
            ]
        );
        assert_eq!(
            sb.selected_row().map(SidebarRow::key),
            Some(session("boxes/i-1", 1))
        );
    }

    #[test]
    fn filter_line_shows_the_query_tail_and_places_the_cursor() {
        let mut sb = filtered("handle");
        sb.rebuild(&local(&store(&["wrk"])), view);
        let area = Rect::new(0, 0, 24, 6);
        let mut buf = Buffer::empty(area);
        let cursor = sb.render(area, &mut buf, &crate::settings::Theme::default());
        assert_eq!(cursor, Some(Position::new(9, 1)));
        assert_eq!(tail_to_width("abcdef", 4), "…def");
        assert_eq!(tail_to_width("abc", 4), "abc");
    }
}
