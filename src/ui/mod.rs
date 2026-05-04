pub mod modal;
pub mod projects;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::pane::Focus;
use crate::pane::terminal::PtyPaneWidget;
use crate::{App, LayoutMode, compute_layout};

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    let body = outer[0];
    let status = outer[1];

    let layout = compute_layout(body, app);

    if let Some(sidebar_area) = layout.sidebar {
        app.sidebar.focused = app.focus == Focus::Projects;
        app.sidebar.render(sidebar_area, frame.buffer_mut(), &app.store);
    }

    let claude_pane = app.active_claude();
    let shell_pane = app.active_shell();

    match app.layout_mode {
        LayoutMode::Split => {
            draw_terminal_pane(
                frame,
                layout.claude,
                " claude ",
                app.focus == Focus::Claude,
                claude_pane,
                "no project selected — press Enter on a project",
            );
            draw_terminal_pane(
                frame,
                layout.shell,
                " shell ",
                app.focus == Focus::Shell,
                shell_pane,
                "no project selected — press Enter on a project",
            );
        }
        LayoutMode::Tabbed => {
            if let Some(strip) = layout.tab_strip {
                draw_tab_strip(frame, strip, app.focus);
            }
            // claude and shell rects point at the same content area; render only the focused one.
            let (title, focused, pane, focused_for_border) = match app.focus {
                Focus::Shell => (" shell ", true, shell_pane, true),
                _ => (" claude ", true, claude_pane, true),
            };
            let _ = focused; // visible pane is implicitly focused
            draw_terminal_pane(
                frame,
                layout.claude,
                title,
                focused_for_border,
                pane,
                "no project selected — press Enter on a project",
            );
        }
    }

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
    match app.focus {
        Focus::Claude => {
            if let Some(pane) = claude_pane {
                let inner = inner_area(layout.claude);
                if let Some(pos) = crate::pane::terminal::cursor_position(pane, inner) {
                    frame.set_cursor_position(pos);
                }
            }
        }
        Focus::Shell => {
            if let Some(pane) = shell_pane {
                let inner = inner_area(layout.shell);
                if let Some(pos) = crate::pane::terminal::cursor_position(pane, inner) {
                    frame.set_cursor_position(pos);
                }
            }
        }
        _ => {}
    }
}

fn draw_tab_strip(frame: &mut Frame, area: Rect, focus: Focus) {
    let half = area.width / 2;
    let claude_rect = Rect {
        x: area.x,
        y: area.y,
        width: half,
        height: 1,
    };
    let shell_rect = Rect {
        x: area.x + half,
        y: area.y,
        width: area.width - half,
        height: 1,
    };
    let claude_focused = focus == Focus::Claude || focus == Focus::Projects;
    frame.render_widget(tab_label("claude", claude_focused), claude_rect);
    frame.render_widget(tab_label("shell", focus == Focus::Shell), shell_rect);
}

fn tab_label(label: &str, active: bool) -> Paragraph<'static> {
    let style = if active {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Paragraph::new(format!(" {label} "))
        .style(style)
        .alignment(ratatui::layout::Alignment::Center)
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
    let session_count = app.sessions.len();
    let layout = match app.layout_mode {
        LayoutMode::Split => "split",
        LayoutMode::Tabbed => "tabs",
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
        Span::styled(
            format!("({session_count} live · {layout}) "),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    if let Some(err) = &app.error {
        spans.push(Span::styled(
            format!("error: {err}"),
            Style::default().fg(Color::Red),
        ));
    } else {
        let hint = match app.focus {
            Focus::Projects => "↑/↓ Enter/dbl-click  +/d/r  /  Alt+0 sidebar  Alt+t tabs  Alt+h/l resize  Alt+q quit",
            _ => "Alt+1/2/3 panes  Alt+0 sidebar  Alt+t tabs  Alt+h/l resize  Alt+q quit",
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
