use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};

use crate::session::DiscoveredSession;

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

    pub fn render(&self, area: Rect, buf: &mut Buffer) -> Position {
        let popup = centered_rect(60, 30, area);
        Clear.render(popup, buf);
        let block = Block::default()
            .title(" add project ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
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
            .style(field_style(self.focus == AddField::Path))
            .render(path_row, buf);

        Paragraph::new("name (optional, defaults to dir basename):").render(layout[2], buf);
        Paragraph::new(self.name_input.as_str())
            .style(field_style(self.focus == AddField::Name))
            .render(name_row, buf);

        let footer = if let Some(err) = &self.error {
            Line::from(vec![Span::styled(
                err.clone(),
                Style::default().fg(Color::Red),
            )])
        } else {
            Line::from("Tab: switch field   Enter: add   Esc: cancel")
        };
        Paragraph::new(footer)
            .style(Style::default().fg(Color::DarkGray))
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
    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        let popup = centered_rect(50, 20, area);
        Clear.render(popup, buf);
        let block = Block::default()
            .title(" confirm delete ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Red));
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
            .style(Style::default().fg(Color::DarkGray))
            .render(layout[2], buf);
    }
}

/// Modal for picking which Claude session to attach to a new tab.
///
/// The list shows discovered on-disk sessions (newest first) plus a
/// synthetic "New session" entry at index 0. The caller reads `confirmed` to
/// learn whether the user accepted; `selected()` returns the chosen session ID
/// (None = new session) and `tab_name` gives the desired display name.
#[derive(Debug, Clone)]
pub struct ClaudeTabPickerModal {
    /// "New session" + discovered sessions (newest first).
    pub sessions: Vec<Option<String>>,
    pub selected_idx: usize,
    pub tab_name: String,
    pub name_focused: bool,
    pub confirmed: bool,
}

impl ClaudeTabPickerModal {
    pub fn new(discovered: &[DiscoveredSession]) -> Self {
        let mut sessions = vec![None]; // "New session"
        for s in discovered {
            sessions.push(Some(s.session_id.clone()));
        }
        Self {
            sessions,
            selected_idx: 0,
            tab_name: String::new(),
            name_focused: false,
            confirmed: false,
        }
    }

    pub fn selected_session_id(&self) -> Option<&str> {
        self.sessions
            .get(self.selected_idx)
            .and_then(|o| o.as_deref())
    }

    pub fn select_next(&mut self) {
        if self.selected_idx + 1 < self.sessions.len() {
            self.selected_idx += 1;
        }
    }

    pub fn select_prev(&mut self) {
        self.selected_idx = self.selected_idx.saturating_sub(1);
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) -> Position {
        let popup = centered_rect(60, 70, area);
        Clear.render(popup, buf);
        let block = Block::default()
            .title(" add claude session ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(popup);
        block.render(popup, buf);

        // Layout: name label, name field, blank, session list, blank, footer
        let list_height = inner.height.saturating_sub(5).max(1);
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // "name:" label
                Constraint::Length(1), // name input
                Constraint::Length(1), // blank
                Constraint::Length(list_height),
                Constraint::Min(0),
                Constraint::Length(1), // footer
            ])
            .split(inner);

        Paragraph::new("tab name (optional):").render(layout[0], buf);
        Paragraph::new(self.tab_name.as_str())
            .style(field_style(self.name_focused))
            .render(layout[1], buf);

        // Session list
        let list_area = layout[3];
        let visible = list_area.height as usize;
        let start = self
            .selected_idx
            .saturating_sub(visible.saturating_sub(1));
        for (i, sess) in self.sessions.iter().enumerate().skip(start).take(visible) {
            let y = list_area.y + (i - start) as u16;
            if y >= list_area.y + list_area.height {
                break;
            }
            let row = Rect { y, height: 1, ..list_area };
            let label = match sess {
                None => "  [ New session ]".to_string(),
                Some(id) => format!("  {}", id),
            };
            let style = if i == self.selected_idx {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Paragraph::new(label).style(style).render(row, buf);
        }

        let footer = if self.name_focused {
            "Enter: confirm   Esc: back to list"
        } else {
            "↑/↓: select   Tab: edit name   Enter: confirm   Esc: cancel"
        };
        Paragraph::new(footer)
            .style(Style::default().fg(Color::DarkGray))
            .render(layout[5], buf);

        // Cursor position — either in the name field or at first list row
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

fn field_style(focused: bool) -> Style {
    if focused {
        Style::default().bg(Color::DarkGray).fg(Color::White)
    } else {
        Style::default().fg(Color::Gray)
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
