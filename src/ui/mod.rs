pub mod modal;
pub mod projects;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::App;
use crate::pane::Focus;
use crate::pane::terminal::PtyPaneWidget;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    let body = outer[0];
    let status = outer[1];

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(24),
            Constraint::Percentage(50),
            Constraint::Min(20),
        ])
        .split(body);
    let sidebar_area = cols[0];
    let claude_area = cols[1];
    let shell_area = cols[2];

    app.sidebar.focused = app.focus == Focus::Projects;
    app.sidebar.render(sidebar_area, frame.buffer_mut(), &app.store);

    draw_terminal_pane(
        frame,
        claude_area,
        " claude ",
        app.focus == Focus::Claude,
        app.claude.as_ref(),
        "no project selected — press Enter on a project",
    );
    draw_terminal_pane(
        frame,
        shell_area,
        " shell ",
        app.focus == Focus::Shell,
        app.shell.as_ref(),
        "no project selected — press Enter on a project",
    );

    draw_status(frame, status, app);

    if let Some(modal) = &app.modal {
        match modal {
            ModalState::Add(m) => {
                let cursor = m.render(area, frame.buffer_mut());
                frame.set_cursor_position(cursor);
            }
            ModalState::ConfirmDelete(m) => {
                m.render(area, frame.buffer_mut());
            }
        }
        return;
    }

    // Position the cursor inside the focused terminal pane.
    if app.modal.is_none() {
        match app.focus {
            Focus::Claude => {
                if let Some(pane) = &app.claude {
                    let inner = inner_area(claude_area);
                    if let Some(pos) =
                        crate::pane::terminal::cursor_position(pane, inner)
                    {
                        frame.set_cursor_position(pos);
                    }
                }
            }
            Focus::Shell => {
                if let Some(pane) = &app.shell {
                    let inner = inner_area(shell_area);
                    if let Some(pos) =
                        crate::pane::terminal::cursor_position(pane, inner)
                    {
                        frame.set_cursor_position(pos);
                    }
                }
            }
            _ => {}
        }
    }
}

fn draw_terminal_pane(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    focused: bool,
    pane: Option<&crate::pane::terminal::PtyPane>,
    placeholder: &str,
) {
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    match pane {
        Some(p) => {
            frame.render_widget(PtyPaneWidget(p), inner);
        }
        None => {
            let para = Paragraph::new(placeholder)
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(para, inner);
        }
    }
}

fn inner_area(area: Rect) -> Rect {
    Block::default().borders(Borders::ALL).inner(area)
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let project = app
        .active_project()
        .map(|p| p.name.as_str())
        .unwrap_or("—");
    let focus = match app.focus {
        Focus::Projects => "projects",
        Focus::Claude => "claude",
        Focus::Shell => "shell",
    };
    let mut spans = vec![
        Span::styled(
            format!(" {project} "),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ),
        Span::raw(" "),
        Span::styled(
            format!("[{focus}] "),
            Style::default().fg(Color::Yellow),
        ),
    ];
    if let Some(err) = &app.error {
        spans.push(Span::styled(
            format!("error: {err}"),
            Style::default().fg(Color::Red),
        ));
    } else {
        let hint = match app.focus {
            Focus::Projects => "↑/↓: move  Enter/dbl-click: open  +: add  d: delete  /: filter  r: reload  Alt+2/3: claude/shell  Alt+q: quit",
            _ => "Alt+1: projects  Alt+2: claude  Alt+3: shell  Ctrl+Space: projects  Alt+q: quit",
        };
        spans.push(Span::styled(hint, Style::default().fg(Color::DarkGray)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[derive(Debug, Clone)]
pub enum ModalState {
    Add(modal::AddProjectModal),
    ConfirmDelete(modal::ConfirmDeleteModal),
}
