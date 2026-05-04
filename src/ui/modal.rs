use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};

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
