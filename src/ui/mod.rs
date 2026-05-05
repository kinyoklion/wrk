pub mod modal;
pub mod projects;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use std::collections::HashMap;
use std::time::Duration;

use crate::pane::Focus;
use crate::pane::terminal::PtyPaneWidget;
use crate::status::{self, HookEvent};
use crate::store::LayoutMode;
use crate::ui::projects::ProjectStatus;
use crate::{App, ClaudeTab, ProjectSession, compute_layout};

const WAITING_THRESHOLD: Duration = Duration::from_millis(500);

fn tab_status(tab: &ClaudeTab) -> ProjectStatus {
    if tab.pane.is_none() {
        return ProjectStatus::None;
    }
    if let Some(event) = status::read_tab_status(&tab.status_id) {
        return match event {
            HookEvent::Notification => ProjectStatus::Attention,
            HookEvent::Stop => ProjectStatus::Waiting,
            HookEvent::UserPromptSubmit => ProjectStatus::Busy,
        };
    }
    if let Some(p) = &tab.pane {
        if p.idle_for() >= WAITING_THRESHOLD {
            return ProjectStatus::Waiting;
        } else {
            return ProjectStatus::Busy;
        }
    }
    ProjectStatus::None
}

fn project_status_for(sessions: &HashMap<String, ProjectSession>, name: &str) -> ProjectStatus {
    let Some(session) = sessions.get(name) else {
        return ProjectStatus::None;
    };
    if session.claude_tabs.is_empty() {
        return ProjectStatus::None;
    }
    // Worst-case across all tabs: Attention > Busy > Waiting > None.
    let mut result = ProjectStatus::None;
    for tab in &session.claude_tabs {
        let s = tab_status(tab);
        result = match (result, s) {
            (_, ProjectStatus::Attention) => ProjectStatus::Attention,
            (ProjectStatus::Attention, _) => ProjectStatus::Attention,
            (_, ProjectStatus::Busy) => ProjectStatus::Busy,
            (ProjectStatus::Busy, _) => ProjectStatus::Busy,
            (_, ProjectStatus::Waiting) => ProjectStatus::Waiting,
            _ => result,
        };
        if result == ProjectStatus::Attention {
            break;
        }
    }
    result
}

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
        let sessions = &app.sessions;
        app.sidebar
            .render(sidebar_area, frame.buffer_mut(), &app.store, |name| {
                project_status_for(sessions, name)
            });
    }

    let session = app.active_session();
    let claude_tabs: Option<&[ClaudeTab]> = session.map(|s| s.claude_tabs.as_slice());
    let active_claude_idx = session.map(|s| s.active_claude).unwrap_or(0);
    let claude_pane = app.active_claude();
    let shell_pane = app.active_shell();

    match app.layout_mode {
        LayoutMode::Split => {
            draw_claude_pane(
                frame,
                layout.claude,
                app.focus == Focus::Claude,
                claude_tabs,
                active_claude_idx,
                claude_pane,
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
            match app.focus {
                Focus::Shell => draw_terminal_pane(
                    frame,
                    layout.claude,
                    " shell ",
                    true,
                    shell_pane,
                    "no project selected — press Enter on a project",
                ),
                _ => draw_claude_pane(
                    frame,
                    layout.claude,
                    true,
                    claude_tabs,
                    active_claude_idx,
                    claude_pane,
                ),
            }
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
            ModalState::ClaudeTabPicker(m) => {
                let cursor = m.render(area, frame.buffer_mut());
                if m.name_focused {
                    frame.set_cursor_position(cursor);
                }
            }
        }
        return;
    }

    // Position the cursor inside the focused terminal pane.
    // For the claude pane we must account for the 1-row tab strip.
    let claude_content_area = {
        let inner = inner_area(layout.claude);
        let has_strip = claude_tabs.is_some_and(|t| !t.is_empty()) && inner.height >= 2;
        if has_strip {
            Rect {
                y: inner.y + 1,
                height: inner.height.saturating_sub(1),
                ..inner
            }
        } else {
            inner
        }
    };
    match app.focus {
        Focus::Claude => {
            if let Some(pane) = claude_pane {
                if let Some(pos) = crate::pane::terminal::cursor_position(pane, claude_content_area)
                {
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

/// Renders the Claude pane: border + optional per-tab strip + terminal content.
fn draw_claude_pane(
    frame: &mut Frame,
    area: Rect,
    focused: bool,
    tabs: Option<&[ClaudeTab]>,
    active_idx: usize,
    pane: Option<&crate::pane::terminal::PtyPane>,
) {
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .title(" claude ")
        .borders(Borders::ALL)
        .border_style(border_style);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let show_tab_strip = tabs.is_some_and(|t| !t.is_empty());

    let (tab_strip_area, content_area) = if show_tab_strip && inner.height >= 2 {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(inner);
        (Some(rows[0]), rows[1])
    } else {
        (None, inner)
    };

    if let (Some(strip_area), Some(tabs)) = (tab_strip_area, tabs) {
        draw_claude_tab_strip(frame, strip_area, tabs, active_idx);
    }

    match pane {
        Some(p) => frame.render_widget(PtyPaneWidget(p), content_area),
        None => {
            let para = Paragraph::new("no project selected — press Enter on a project")
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(para, content_area);
        }
    }
}

/// Tab strip for Claude sessions within a project.
fn draw_claude_tab_strip(frame: &mut Frame, area: Rect, tabs: &[ClaudeTab], active_idx: usize) {
    if tabs.is_empty() || area.width == 0 {
        return;
    }
    let tab_width = (area.width as usize / tabs.len()).max(1) as u16;
    for (i, tab) in tabs.iter().enumerate() {
        let x = area.x + (i as u16) * tab_width;
        if x >= area.x + area.width {
            break;
        }
        let w = if i + 1 == tabs.len() {
            area.x + area.width - x
        } else {
            tab_width
        };
        let tab_rect = Rect {
            x,
            y: area.y,
            width: w,
            height: 1,
        };
        let status_char = match tab_status(tab) {
            ProjectStatus::Attention => "● ",
            ProjectStatus::Busy => "· ",
            ProjectStatus::Waiting => "● ",
            ProjectStatus::None => "  ",
        };
        let status_color = match tab_status(tab) {
            ProjectStatus::Attention => Color::Red,
            ProjectStatus::Busy => Color::Yellow,
            ProjectStatus::Waiting => Color::Green,
            ProjectStatus::None => Color::DarkGray,
        };
        let label = format!("{status_char}{}", tab.name);
        let style = if i == active_idx {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(status_color)
        };
        let spans = if i == active_idx {
            Line::from(vec![Span::raw(label)])
        } else {
            Line::from(vec![
                Span::styled(status_char.to_string(), Style::default().fg(status_color)),
                Span::styled(tab.name.clone(), Style::default().fg(Color::DarkGray)),
            ])
        };
        frame.render_widget(
            Paragraph::new(spans)
                .style(style)
                .alignment(ratatui::layout::Alignment::Left),
            tab_rect,
        );
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
            let para = Paragraph::new(placeholder).style(Style::default().fg(Color::DarkGray));
            frame.render_widget(para, inner);
        }
    }
}

fn inner_area(area: Rect) -> Rect {
    Block::default().borders(Borders::ALL).inner(area)
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let project = app.active_project().map(|p| p.name.as_str()).unwrap_or("—");
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
        Span::styled(format!("[{focus}] "), Style::default().fg(Color::Yellow)),
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
            Focus::Projects => {
                "↑/↓ Enter/dbl-click  +/d/r  /  Alt+0 sidebar  Alt+t tabs  Alt+h/l resize  Alt+q quit"
            }
            _ => "Alt+1/2/3  Alt+n new-claude  Alt+w close  Alt+</> tabs  Alt+t layout  Alt+q quit",
        };
        spans.push(Span::styled(hint, Style::default().fg(Color::DarkGray)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[derive(Debug, Clone)]
pub enum ModalState {
    Add(modal::AddProjectModal),
    ConfirmDelete(modal::ConfirmDeleteModal),
    ClaudeTabPicker(modal::ClaudeTabPickerModal),
}
