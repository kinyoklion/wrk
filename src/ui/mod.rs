pub mod modal;
pub mod projects;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use std::collections::HashMap;
use std::time::Duration;

use crate::pane::Focus;
use crate::pane::terminal::PtyPaneWidget;
use crate::settings::Theme;
use crate::status::{self, HookEvent};
use crate::store::LayoutMode;
use crate::ui::projects::ProjectStatus;
use crate::{App, ClaudeTab, ProjectSession, claude_pane_split, compute_layout};

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
    let theme = app.theme;

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
        app.sidebar.render(
            sidebar_area,
            frame.buffer_mut(),
            &app.store,
            &theme,
            |name| project_status_for(sessions, name),
            |name| sessions.contains_key(name),
        );
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
                &theme,
            );
            draw_terminal_pane(
                frame,
                layout.shell,
                " shell ",
                app.focus == Focus::Shell,
                shell_pane,
                "no project selected — press Enter on a project",
                &theme,
            );
        }
        LayoutMode::Tabbed => {
            if let Some(strip) = layout.tab_strip {
                draw_tab_strip(frame, strip, app.focus, &theme);
            }
            match app.focus {
                Focus::Shell => draw_terminal_pane(
                    frame,
                    layout.claude,
                    " shell ",
                    true,
                    shell_pane,
                    "no project selected — press Enter on a project",
                    &theme,
                ),
                _ => draw_claude_pane(
                    frame,
                    layout.claude,
                    true,
                    claude_tabs,
                    active_claude_idx,
                    claude_pane,
                    &theme,
                ),
            }
        }
    }

    draw_status(frame, status, app);

    if let Some(modal) = &app.modal {
        match modal {
            ModalState::Add(m) => {
                let cursor = m.render(area, frame.buffer_mut(), &theme);
                frame.set_cursor_position(cursor);
            }
            ModalState::ConfirmDelete(m) => {
                m.render(area, frame.buffer_mut(), &theme);
            }
            ModalState::ConfirmUnload(m) => {
                m.render(area, frame.buffer_mut(), &theme);
            }
            ModalState::ClaudeTabPicker(m) => {
                let cursor = m.render(area, frame.buffer_mut(), &theme);
                if m.name_focused {
                    frame.set_cursor_position(cursor);
                }
            }
            ModalState::UrlPicker(m) => {
                let cursor = m.render(area, frame.buffer_mut(), &theme);
                frame.set_cursor_position(cursor);
            }
        }
        return;
    }

    // Position the cursor inside the focused terminal pane.
    // For the claude pane we must account for the 1-row tab strip.
    let claude_content_area = {
        let inner = inner_area(layout.claude);
        let count = claude_tabs.map(|t| t.len()).unwrap_or(0);
        claude_pane_split(inner, count).1
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
    theme: &Theme,
) {
    let border_style = Style::default().fg(if focused {
        theme.border_focused
    } else {
        theme.border_unfocused
    });
    let block = Block::default()
        .title(" claude ")
        .borders(Borders::ALL)
        .border_style(border_style);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let tab_count = tabs.map(|t| t.len()).unwrap_or(0);
    let (tab_strip_area, content_area) = claude_pane_split(inner, tab_count);

    if let (Some(strip_area), Some(tabs)) = (tab_strip_area, tabs) {
        draw_claude_tab_strip(frame, strip_area, tabs, active_idx, theme);
    }

    match pane {
        Some(p) => frame.render_widget(PtyPaneWidget(p), content_area),
        None => {
            let para = Paragraph::new("no project selected — press Enter on a project")
                .style(Style::default().fg(theme.hint));
            frame.render_widget(para, content_area);
        }
    }
}

/// Tab strip for Claude sessions within a project.
fn draw_claude_tab_strip(
    frame: &mut Frame,
    area: Rect,
    tabs: &[ClaudeTab],
    active_idx: usize,
    theme: &Theme,
) {
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
            ProjectStatus::Attention => theme.status_attention,
            ProjectStatus::Busy => theme.status_busy,
            ProjectStatus::Waiting => theme.status_waiting,
            ProjectStatus::None => theme.hint,
        };
        let label = format!("{status_char}{}", tab.name);
        let style = if i == active_idx {
            Style::default()
                .fg(theme.accent_fg)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(status_color)
        };
        let spans = if i == active_idx {
            Line::from(vec![Span::raw(label)])
        } else {
            Line::from(vec![
                Span::styled(status_char.to_string(), Style::default().fg(status_color)),
                Span::styled(tab.name.clone(), Style::default().fg(theme.hint)),
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

fn draw_tab_strip(frame: &mut Frame, area: Rect, focus: Focus, theme: &Theme) {
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
    frame.render_widget(tab_label("claude", claude_focused, theme), claude_rect);
    frame.render_widget(tab_label("shell", focus == Focus::Shell, theme), shell_rect);
}

fn tab_label(label: &str, active: bool, theme: &Theme) -> Paragraph<'static> {
    let style = if active {
        Style::default()
            .fg(theme.accent_fg)
            .bg(theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.hint)
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
    theme: &Theme,
) {
    let border_style = Style::default().fg(if focused {
        theme.border_focused
    } else {
        theme.border_unfocused
    });
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
            let para = Paragraph::new(placeholder).style(Style::default().fg(theme.hint));
            frame.render_widget(para, inner);
        }
    }
}

fn inner_area(area: Rect) -> Rect {
    Block::default().borders(Borders::ALL).inner(area)
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
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
            Style::default().fg(theme.accent_fg).bg(theme.accent),
        ),
        Span::raw(" "),
        Span::styled(
            format!("[{focus}] "),
            Style::default().fg(theme.focus_indicator),
        ),
        Span::styled(
            format!("({session_count} live · {layout}) "),
            Style::default().fg(theme.hint),
        ),
    ];
    if app.shell_passthrough {
        // Reverse-video the passthrough chip so it stands out — a reminder
        // that wrk's normal Alt+… shortcuts won't fire while focus is on the
        // shell pane. Reuses theme slots so users who recolor `error` get a
        // matching chip background.
        spans.push(Span::styled(
            "[passthru] ",
            Style::default()
                .fg(theme.accent_fg)
                .bg(theme.error)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if app.select_mode {
        spans.push(Span::styled(
            "[select] ",
            Style::default()
                .fg(theme.accent_fg)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(err) = &app.error {
        spans.push(Span::styled(
            format!("error: {err}"),
            Style::default().fg(theme.error),
        ));
    } else if let Some(info) = &app.info {
        // Transient feedback like "copied N chars" — no prefix, hint-colored
        // so it reads as status rather than an alert. Cleared on the next
        // key or mouse event.
        spans.push(Span::styled(info.clone(), Style::default().fg(theme.info)));
    } else {
        let hint = build_hint(app);
        spans.push(Span::styled(hint, Style::default().fg(theme.hint)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Compose the focus-specific hint string from the live keymap so it stays in
/// sync with any user overrides. Hard-coded keys (the projects-pane single
/// chars `+`, `d`, `r`, `/`, `↑`/`↓`, `Enter`) are not configurable yet so
/// they're spliced in literally.
fn build_hint(app: &App) -> String {
    let km = &app.keymap;
    if app.select_mode {
        return format!(
            "select: drag to highlight, release to copy, {esc}/{toggle} cancel",
            esc = "Esc",
            toggle = km.display(crate::keymap::GlobalAction::EnterSelectMode),
        );
    }
    let passthrough_active = app.shell_passthrough && app.focus == Focus::Shell;
    if passthrough_active {
        return format!(
            "{toggle} exit passthrough  (all other keys → shell)",
            toggle = km.display(crate::keymap::GlobalAction::ToggleShellPassthrough),
        );
    }
    use crate::keymap::GlobalAction as A;
    let toggle_sidebar = km.display(A::ToggleSidebar);
    let toggle_layout = km.display(A::ToggleLayout);
    let shrink = km.display(A::ShrinkClaude);
    let grow = km.display(A::GrowClaude);
    let quit = km.display(A::Quit);
    match app.focus {
        Focus::Projects => format!(
            "↑/↓ Enter/dbl-click  +/d/u/r  /  {toggle_sidebar} sidebar  \
             {toggle_layout} layout  {shrink}/{grow} resize  {quit} quit",
        ),
        _ => format!(
            "{focus_p}/{focus_c}/{focus_s} panes  {new} new-claude  \
             {close} close  {prev}/{next} tabs  {toggle_layout} layout  \
             {passthru} passthru  {quit} quit",
            focus_p = km.display(A::FocusProjects),
            focus_c = km.display(A::FocusClaude),
            focus_s = km.display(A::FocusShell),
            new = km.display(A::NewClaudeTab),
            close = km.display(A::CloseClaudeTab),
            prev = km.display(A::PrevClaudeTab),
            next = km.display(A::NextClaudeTab),
            passthru = km.display(A::ToggleShellPassthrough),
        ),
    }
}

#[derive(Debug, Clone)]
pub enum ModalState {
    Add(modal::AddProjectModal),
    ConfirmDelete(modal::ConfirmDeleteModal),
    ConfirmUnload(modal::ConfirmUnloadModal),
    ClaudeTabPicker(modal::ClaudeTabPickerModal),
    UrlPicker(modal::UrlPickerModal),
}
