use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};

use crate::session::DiscoveredSession;
use crate::settings::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddField {
    Path,
    Name,
}

#[derive(Debug, Clone)]
pub struct AddProjectModal {
    pub path_input: String,
    pub name_input: String,
    pub focus: AddField,
    pub error: Option<String>,
}

impl Default for AddProjectModal {
    fn default() -> Self {
        Self {
            path_input: String::new(),
            name_input: String::new(),
            focus: AddField::Path,
            error: None,
        }
    }
}

impl AddProjectModal {
    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            AddField::Path => AddField::Name,
            AddField::Name => AddField::Path,
        };
    }

    pub fn current_input_mut(&mut self) -> &mut String {
        match self.focus {
            AddField::Path => &mut self.path_input,
            AddField::Name => &mut self.name_input,
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer, theme: &Theme) -> Position {
        let popup = centered_rect(60, 30, area);
        Clear.render(popup, buf);
        let block = Block::default()
            .title(" add project ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border_focused));
        let inner = block.inner(popup);
        block.render(popup, buf);

        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(inner);

        let (path_row, name_row) = (layout[1], layout[3]);
        Paragraph::new("path:").render(layout[0], buf);
        Paragraph::new(self.path_input.as_str())
            .style(field_style(self.focus == AddField::Path, theme))
            .render(path_row, buf);

        Paragraph::new("name (optional, defaults to dir basename):").render(layout[2], buf);
        Paragraph::new(self.name_input.as_str())
            .style(field_style(self.focus == AddField::Name, theme))
            .render(name_row, buf);

        let footer = if let Some(err) = &self.error {
            Line::from(vec![Span::styled(
                err.clone(),
                Style::default().fg(theme.error),
            )])
        } else {
            Line::from("Tab: switch field   Enter: add   Esc: cancel")
        };
        Paragraph::new(footer)
            .style(Style::default().fg(theme.hint))
            .render(layout[5], buf);

        let cursor_row = match self.focus {
            AddField::Path => path_row,
            AddField::Name => name_row,
        };
        let len = match self.focus {
            AddField::Path => self.path_input.chars().count() as u16,
            AddField::Name => self.name_input.chars().count() as u16,
        };
        Position {
            x: cursor_row.x + len.min(cursor_row.width.saturating_sub(1)),
            y: cursor_row.y,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfirmDeleteModal {
    pub project_name: String,
}

impl ConfirmDeleteModal {
    pub fn render(&self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        let popup = centered_rect(50, 20, area);
        Clear.render(popup, buf);
        let block = Block::default()
            .title(" confirm delete ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.error));
        let inner = block.inner(popup);
        block.render(popup, buf);

        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(inner);

        Paragraph::new(format!("delete project '{}'?", self.project_name)).render(layout[0], buf);
        Paragraph::new("y: confirm   n / Esc: cancel")
            .style(Style::default().fg(theme.hint))
            .render(layout[2], buf);
    }
}

#[derive(Debug, Clone)]
pub struct ConfirmUnloadModal {
    pub project_name: String,
}

impl ConfirmUnloadModal {
    pub fn render(&self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        let popup = centered_rect(50, 20, area);
        Clear.render(popup, buf);
        let block = Block::default()
            .title(" confirm unload ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.error));
        let inner = block.inner(popup);
        block.render(popup, buf);

        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(inner);

        Paragraph::new(format!(
            "unload '{}'? (kills its claude + shell)",
            self.project_name
        ))
        .render(layout[0], buf);
        Paragraph::new("y: confirm   n / Esc: cancel")
            .style(Style::default().fg(theme.hint))
            .render(layout[2], buf);
    }
}

/// Modal for picking which Claude session to attach to a new tab.
///
/// Index 0 is always the synthetic "New session" entry. Indices ≥ 1 are
/// discovered on-disk sessions (newest first). The caller checks `confirmed`
/// then reads `selected_session_id()` (None = new) and `tab_name`.
#[derive(Debug, Clone)]
pub struct ClaudeTabPickerModal {
    /// None = "New session"; Some = a discovered session.
    pub sessions: Vec<Option<DiscoveredSession>>,
    pub selected_idx: usize,
    pub tab_name: String,
    pub name_focused: bool,
    pub confirmed: bool,
}

impl ClaudeTabPickerModal {
    pub fn new(discovered: &[DiscoveredSession]) -> Self {
        let mut sessions: Vec<Option<DiscoveredSession>> = vec![None];
        for s in discovered {
            sessions.push(Some(s.clone()));
        }
        Self {
            sessions,
            selected_idx: 0,
            tab_name: String::new(),
            name_focused: false,
            confirmed: false,
        }
    }

    /// Session ID of the selected entry, or `None` for "New session".
    pub fn selected_session_id(&self) -> Option<&str> {
        self.sessions
            .get(self.selected_idx)
            .and_then(|o| o.as_ref())
            .map(|s| s.session_id.as_str())
    }

    /// Suggested tab name: the session's stored name if one exists, otherwise
    /// the first 8 chars of the session ID, or "new" for a new session.
    pub fn suggested_name(&self) -> String {
        match self
            .sessions
            .get(self.selected_idx)
            .and_then(|o| o.as_ref())
        {
            Some(s) => s
                .name
                .clone()
                .unwrap_or_else(|| s.session_id[..s.session_id.len().min(8)].to_string()),
            None => "new".to_string(),
        }
    }

    pub fn select_next(&mut self) {
        if self.selected_idx + 1 < self.sessions.len() {
            self.selected_idx += 1;
        }
    }

    pub fn select_prev(&mut self) {
        self.selected_idx = self.selected_idx.saturating_sub(1);
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer, theme: &Theme) -> Position {
        let popup = centered_rect(60, 70, area);
        Clear.render(popup, buf);
        let block = Block::default()
            .title(" add claude session ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border_focused));
        let inner = block.inner(popup);
        block.render(popup, buf);

        // Layout: name label + field, blank, session list, footer
        let list_height = inner.height.saturating_sub(5).max(1);
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // label
                Constraint::Length(1), // name input
                Constraint::Length(1), // blank
                Constraint::Length(list_height),
                Constraint::Min(0),
                Constraint::Length(1), // footer
            ])
            .split(inner);

        let name_hint = if self.name_focused {
            "tab name:"
        } else {
            "tab name (Tab to edit):"
        };
        Paragraph::new(name_hint).render(layout[0], buf);
        let display_name = if self.name_focused || !self.tab_name.is_empty() {
            self.tab_name.as_str()
        } else {
            // show suggested name greyed out when field is empty and unfocused
            ""
        };
        let name_style = if self.name_focused {
            field_style(true, theme)
        } else if self.tab_name.is_empty() {
            Style::default().fg(theme.hint)
        } else {
            field_style(false, theme)
        };
        let name_display = if !self.name_focused && self.tab_name.is_empty() {
            self.suggested_name()
        } else {
            display_name.to_string()
        };
        Paragraph::new(name_display)
            .style(name_style)
            .render(layout[1], buf);

        // Session list
        let list_area = layout[3];
        let visible = list_area.height as usize;
        let start = self.selected_idx.saturating_sub(visible.saturating_sub(1));
        for (i, sess) in self.sessions.iter().enumerate().skip(start).take(visible) {
            let y = list_area.y + (i - start) as u16;
            if y >= list_area.y + list_area.height {
                break;
            }
            let row = Rect {
                y,
                height: 1,
                ..list_area
            };
            let label = match sess {
                None => "  [ New session ]".to_string(),
                Some(s) => match &s.name {
                    Some(name) => format!(
                        "  {name}  ({}…)",
                        &s.session_id[..8.min(s.session_id.len())]
                    ),
                    None => format!("  {}", s.session_id),
                },
            };
            let style = if i == self.selected_idx {
                Style::default()
                    .fg(theme.accent_fg)
                    .bg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Paragraph::new(label).style(style).render(row, buf);
        }

        let footer = if self.name_focused {
            "Enter/Esc: back to list"
        } else {
            "↑/↓: select   Tab: name   Enter: confirm   Esc: cancel"
        };
        Paragraph::new(footer)
            .style(Style::default().fg(theme.hint))
            .render(layout[5], buf);

        if self.name_focused {
            let len = self.tab_name.chars().count() as u16;
            Position {
                x: layout[1].x + len.min(layout[1].width.saturating_sub(1)),
                y: layout[1].y,
            }
        } else {
            Position {
                x: list_area.x,
                y: list_area.y,
            }
        }
    }
}

/// Modal for picking a URL from a pane's scrollback. The constructor receives
/// the candidate list newest-first; the modal owns the substring filter and
/// selection state, and the caller reads `confirmed_url` after Enter.
#[derive(Debug, Clone)]
pub struct UrlPickerModal {
    pub all: Vec<String>,
    pub query: String,
    pub filtered: Vec<usize>,
    pub selected: usize,
    pub confirmed_url: Option<String>,
}

impl UrlPickerModal {
    pub fn new(urls: Vec<String>) -> Self {
        let filtered = (0..urls.len()).collect();
        Self {
            all: urls,
            query: String::new(),
            filtered,
            selected: 0,
            confirmed_url: None,
        }
    }

    pub fn select_next(&mut self) {
        if self.selected + 1 < self.filtered.len() {
            self.selected += 1;
        }
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.refilter();
    }

    pub fn pop_char(&mut self) {
        if self.query.pop().is_some() {
            self.refilter();
        }
    }

    /// Case-insensitive substring match. Keeps the newest-first ordering.
    fn refilter(&mut self) {
        let needle = self.query.to_lowercase();
        self.filtered = self
            .all
            .iter()
            .enumerate()
            .filter(|(_, u)| needle.is_empty() || u.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        self.selected = 0;
    }

    /// Stash the currently highlighted URL into `confirmed_url`; the caller
    /// reads it after the modal is popped.
    pub fn confirm(&mut self) {
        if let Some(&idx) = self.filtered.get(self.selected) {
            self.confirmed_url = self.all.get(idx).cloned();
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer, theme: &Theme) -> Position {
        let popup = centered_rect(70, 60, area);
        Clear.render(popup, buf);
        let block = Block::default()
            .title(" open url ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border_focused));
        let inner = block.inner(popup);
        block.render(popup, buf);

        let list_height = inner.height.saturating_sub(4).max(1);
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // label
                Constraint::Length(1), // query input
                Constraint::Length(1), // blank
                Constraint::Length(list_height),
                Constraint::Min(0),
                Constraint::Length(1), // footer
            ])
            .split(inner);

        Paragraph::new("filter:").render(layout[0], buf);
        Paragraph::new(self.query.as_str())
            .style(field_style(true, theme))
            .render(layout[1], buf);

        let list_area = layout[3];
        let visible = list_area.height as usize;
        let start = self.selected.saturating_sub(visible.saturating_sub(1));
        for (offset, idx) in self.filtered.iter().enumerate().skip(start).take(visible) {
            let y = list_area.y + (offset - start) as u16;
            if y >= list_area.y + list_area.height {
                break;
            }
            let row = Rect {
                y,
                height: 1,
                ..list_area
            };
            // Truncate long URLs so they fit on one row.
            let url = self.all.get(*idx).map(String::as_str).unwrap_or("");
            let max = row.width.saturating_sub(2) as usize;
            let display: String = if url.chars().count() > max {
                let mut s: String = url.chars().take(max.saturating_sub(1)).collect();
                s.push('…');
                format!("  {s}")
            } else {
                format!("  {url}")
            };
            let style = if offset == self.selected {
                Style::default()
                    .fg(theme.accent_fg)
                    .bg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Paragraph::new(display).style(style).render(row, buf);
        }

        if self.filtered.is_empty() && !self.all.is_empty() {
            let msg = Paragraph::new("(no matches)").style(Style::default().fg(theme.hint));
            msg.render(list_area, buf);
        }

        Paragraph::new("type to filter   ↑/↓: select   Enter: open   Esc: cancel")
            .style(Style::default().fg(theme.hint))
            .render(layout[5], buf);

        let len = self.query.chars().count() as u16;
        Position {
            x: layout[1].x + len.min(layout[1].width.saturating_sub(1)),
            y: layout[1].y,
        }
    }
}

fn field_style(focused: bool, theme: &Theme) -> Style {
    if focused {
        // A subtle "this field is being edited" treatment: light text on a
        // darker bg so it pops against the modal interior. Defaults match the
        // pre-theme behavior (Color::White on Color::DarkGray); themes can
        // dial both via `info` and `border_unfocused`.
        Style::default()
            .bg(theme.border_unfocused)
            .fg(ratatui::style::Color::White)
    } else {
        Style::default().fg(theme.info)
    }
}

fn centered_rect(width_pct: u16, height_pct: u16, area: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_pct) / 2),
            Constraint::Percentage(height_pct),
            Constraint::Percentage((100 - height_pct) / 2),
        ])
        .split(area);
    let h = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_pct) / 2),
            Constraint::Percentage(width_pct),
            Constraint::Percentage((100 - width_pct) / 2),
        ])
        .split(v[1]);
    h[1]
}
