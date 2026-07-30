pub mod modal;
pub mod projects;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use std::collections::HashMap;
use std::time::Duration;

use crate::pane::Focus;
use crate::pane::terminal::PtyPaneWidget;
use crate::settings::Theme;
use crate::status::HookEvent;
use crate::store::LayoutMode;
use crate::ui::projects::ProjectStatus;
use crate::{App, ClaudeTab, ProjectSession, Tab, claude_pane_split, compute_layout};
use wrk_markdown::MarkdownView;

const WAITING_THRESHOLD: Duration = Duration::from_millis(500);

fn tab_status(tab: &ClaudeTab) -> ProjectStatus {
    if tab.pane.is_none() {
        return ProjectStatus::None;
    }
    // Precise hook state, once any hook has fired for this tab.
    if let Some(event) = tab.status.event {
        return match event {
            HookEvent::Waiting => ProjectStatus::Attention,
            HookEvent::Stopped => ProjectStatus::Waiting,
            HookEvent::Busy => ProjectStatus::Busy,
        };
    }
    // Fallback: idle-time heuristic until the first hook arrives.
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
    if session.claude_tabs().next().is_none() {
        return ProjectStatus::None;
    }
    // Worst-case across all tabs: Attention > Busy > Waiting > None.
    let mut result = ProjectStatus::None;
    for tab in session.claude_tabs() {
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

    // The fullscreen image viewer takes over the whole screen when open.
    if app.image_viewer.is_some() {
        draw_image_viewer(frame, app, area);
        return;
    }

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

    const SHELL_PLACEHOLDER: &str = "no project selected — press Enter on a project";

    match app.layout_mode {
        LayoutMode::Split => {
            let focused = app.focus == Focus::Claude;
            draw_primary_pane(frame, layout.claude, focused, app, &theme);
            let shell_focused = app.focus == Focus::Shell;
            let shell_pane = app.active_shell();
            draw_terminal_pane(
                frame,
                layout.shell,
                " shell ",
                shell_focused,
                shell_pane,
                SHELL_PLACEHOLDER,
                &theme,
            );
        }
        LayoutMode::Tabbed => {
            if let Some(strip) = layout.tab_strip {
                draw_tab_strip(frame, strip, app.focus, &theme);
            }
            match app.focus {
                Focus::Shell => {
                    let shell_pane = app.active_shell();
                    draw_terminal_pane(
                        frame,
                        layout.claude,
                        " shell ",
                        true,
                        shell_pane,
                        SHELL_PLACEHOLDER,
                        &theme,
                    );
                }
                _ => draw_primary_pane(frame, layout.claude, true, app, &theme),
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
            ModalState::OpenMarkdown(m) => {
                let cursor = m.render(area, frame.buffer_mut(), &theme);
                frame.set_cursor_position(cursor);
            }
            ModalState::ConfirmDelete(m) => {
                m.render(area, frame.buffer_mut(), &theme);
            }
            ModalState::ConfirmUnload(m) => {
                m.render(area, frame.buffer_mut(), &theme);
            }
            ModalState::ConfirmQuit(m) => {
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
    // For the primary pane we must account for the 1-row tab strip. When the
    // active primary tab is a markdown viewer there's no PTY (and no cursor):
    // `active_claude()` returns `None` in that case, so the cursor is left alone.
    let primary_content_area = {
        let inner = inner_area(layout.claude);
        let count = app.active_session().map(|s| s.tabs.len()).unwrap_or(0);
        claude_pane_split(inner, count).1
    };
    match app.focus {
        Focus::Claude => {
            if let Some(pane) = app.active_claude() {
                if let Some(pos) =
                    crate::pane::terminal::cursor_position(pane, primary_content_area)
                {
                    frame.set_cursor_position(pos);
                }
            }
        }
        Focus::Shell => {
            if let Some(pane) = app.active_shell() {
                let inner = inner_area(layout.shell);
                if let Some(pos) = crate::pane::terminal::cursor_position(pane, inner) {
                    frame.set_cursor_position(pos);
                }
            }
        }
        _ => {}
    }
}

/// Render the fullscreen zoom/pan image viewer over the whole screen: the image
/// fills all but the bottom row, which shows the controls + current zoom.
fn draw_image_viewer(frame: &mut Frame, app: &mut App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    let (img_area, hint_area) = (rows[0], rows[1]);

    frame.render_widget(Clear, img_area);
    let mut zoom = 1.0;
    {
        let buf = frame.buffer_mut();
        if let (Some(viewer), Some(picker)) = (app.image_viewer.as_mut(), app.picker.as_ref()) {
            viewer.render(img_area, buf, picker);
            zoom = viewer.zoom();
        }
    }
    let hint = format!(
        " image · +/-/wheel zoom ({:.0}%) · hjkl/arrows pan · 0 reset · q/Esc close ",
        zoom * 100.0
    );
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(app.theme.hint)),
        hint_area,
    );
}

/// Renders the primary pane: border + tab strip + the active tab's content
/// (a Claude PTY or a markdown viewer). Takes `&mut App` because the markdown
/// view is a stateful widget needing `&mut` scroll state.
fn draw_primary_pane(frame: &mut Frame, area: Rect, focused: bool, app: &mut App, theme: &Theme) {
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

    // Snapshot tab geometry/state immutably before any mutable borrow below.
    let (tab_count, active_idx, active_is_md, has_session) = match app.active_session() {
        Some(s) => (
            s.tabs.len(),
            s.active_tab,
            s.current().map(Tab::is_markdown).unwrap_or(false),
            true,
        ),
        None => (0, 0, false, false),
    };
    let (tab_strip_area, content_area) = claude_pane_split(inner, tab_count);

    if let Some(strip_area) = tab_strip_area {
        if let Some(s) = app.active_session() {
            draw_primary_tab_strip(frame, strip_area, &s.tabs, active_idx, theme);
        }
    }

    let placeholder = |frame: &mut Frame| {
        let para = Paragraph::new("no project selected — press Enter on a project")
            .style(Style::default().fg(theme.hint));
        frame.render_widget(para, content_area);
    };

    if !has_session {
        placeholder(frame);
    } else if active_is_md {
        // Borrow `picker` and `sessions` as disjoint fields (not via a `&mut
        // self` method) so the image picker stays reachable while the session
        // is mutably borrowed for the stateful markdown widget.
        let picker = app.picker.as_ref();
        let name = app.active_project_name.clone();
        if let Some(s) = name.as_deref().and_then(|n| app.sessions.get_mut(n)) {
            if let Some(Tab::Markdown(md)) = s.tabs.get_mut(active_idx) {
                // Lay tables out to the current pane width (re-renders on resize).
                // On a re-render, rebuild image protocols for the new width.
                if md.ensure_rendered(content_area.width) {
                    if let Some(picker) = picker {
                        md.state
                            .prepare_images(&md.rendered, picker, content_area.width);
                    }
                }
                // Disjoint field borrows: `&md.rendered` and `&mut md.state`.
                frame.render_stateful_widget(
                    MarkdownView::new(&md.rendered),
                    content_area,
                    &mut md.state,
                );
            }
        }
    } else {
        match app.active_claude() {
            Some(p) => frame.render_widget(PtyPaneWidget(p), content_area),
            None => placeholder(frame),
        }
    }
}

/// Tab strip for a project's primary pane: Claude tabs show a status dot, a
/// markdown tab shows a document glyph and its filename.
fn draw_primary_tab_strip(
    frame: &mut Frame,
    area: Rect,
    tabs: &[Tab],
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
        let (status_char, status_color) = match tab {
            Tab::Claude(c) => {
                let st = tab_status(c);
                let ch = match st {
                    ProjectStatus::Attention => "● ",
                    ProjectStatus::Busy => "· ",
                    ProjectStatus::Waiting => "● ",
                    ProjectStatus::None => "  ",
                };
                let col = match st {
                    ProjectStatus::Attention => theme.status_attention,
                    ProjectStatus::Busy => theme.status_busy,
                    ProjectStatus::Waiting => theme.status_waiting,
                    ProjectStatus::None => theme.hint,
                };
                (ch, col)
            }
            Tab::Markdown(_) => ("▤ ", theme.hint),
        };
        let name = tab.name();
        let style = if i == active_idx {
            Style::default()
                .fg(theme.accent_fg)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(status_color)
        };
        let spans = if i == active_idx {
            Line::from(vec![Span::raw(format!("{status_char}{name}"))])
        } else {
            Line::from(vec![
                Span::styled(status_char.to_string(), Style::default().fg(status_color)),
                Span::styled(name.to_string(), Style::default().fg(theme.hint)),
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

/// The trailing message on the status line. The hint carries several tiers
/// (longest first) so it can collapse to fit a narrow window; errors and info
/// are single strings that truncate with an ellipsis instead of collapsing.
enum StatusMsg {
    Error(String),
    Info(String),
    Hint(Vec<String>),
}

/// Everything the status line renders, independent of `App` so the fitting
/// logic ([`compose_status`]) is unit-testable.
struct StatusView<'a> {
    project: &'a str,
    focus_label: &'a str,
    session_count: usize,
    layout_label: &'a str,
    passthrough: bool,
    select: bool,
    msg: StatusMsg,
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let project = app.active_project().map(|p| p.name.as_str()).unwrap_or("—");
    let focus_label = match app.focus {
        Focus::Projects => "projects",
        Focus::Claude => "claude",
        Focus::Shell => "shell",
    };
    let layout_label = match app.layout_mode {
        LayoutMode::Split => "split",
        LayoutMode::Tabbed => "tabs",
    };
    let msg = if let Some(err) = &app.error {
        StatusMsg::Error(format!("error: {err}"))
    } else if let Some(info) = &app.info {
        // Transient feedback like "copied N chars" — no prefix, hint-colored
        // so it reads as status rather than an alert. Cleared on the next
        // key or mouse event.
        StatusMsg::Info(info.clone())
    } else {
        StatusMsg::Hint(hint_tiers(app))
    };
    let view = StatusView {
        project,
        focus_label,
        session_count: app.sessions.len(),
        layout_label,
        passthrough: app.shell_passthrough,
        select: app.select_mode,
        msg,
    };
    let spans = compose_status(&app.theme, &view, area.width as usize);
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Fit the status line to `width`, dropping the lowest-priority pieces first so
/// the essentials always stay visible. Priority, high to low: the project chip
/// and any active `[passthru]`/`[select]` warning chips (never dropped); then a
/// message (the hint collapses through its tiers, an error/info truncates with
/// `…`); then the `[focus]` and `(N live · layout)` context chips, which are
/// dropped first when space runs low.
fn compose_status(theme: &Theme, v: &StatusView, width: usize) -> Vec<Span<'static>> {
    let project_chip = format!(" {} ", v.project);
    let focus_chip = format!("[{}] ", v.focus_label);
    let live_chip = format!("({} live · {}) ", v.session_count, v.layout_label);
    const GAP: &str = " "; // separator after the colored project chip

    let (tiers, msg_style): (Vec<String>, Style) = match &v.msg {
        StatusMsg::Error(e) => (vec![e.clone()], Style::default().fg(theme.error)),
        StatusMsg::Info(i) => (vec![i.clone()], Style::default().fg(theme.info)),
        StatusMsg::Hint(t) => (t.clone(), Style::default().fg(theme.hint)),
    };
    // Reserve room for at least the shortest message tier when deciding whether
    // the optional context chips fit.
    let min_msg_w = tiers.last().map(|t| cell_width(t)).unwrap_or(0);

    // Mandatory: project chip + gap + any active warning chips.
    let mut mandatory = cell_width(&project_chip) + cell_width(GAP);
    if v.passthrough {
        mandatory += cell_width("[passthru] ");
    }
    if v.select {
        mandatory += cell_width("[select] ");
    }

    // Optional context, added only if the shortest message still fits after.
    let mut opt_budget = width.saturating_sub(mandatory).saturating_sub(min_msg_w);
    let show_focus = cell_width(&focus_chip) <= opt_budget;
    if show_focus {
        opt_budget -= cell_width(&focus_chip);
    }
    let show_live = cell_width(&live_chip) <= opt_budget;

    // Whatever remains after the mandatory + shown context goes to the message.
    let mut consumed = mandatory;
    if show_focus {
        consumed += cell_width(&focus_chip);
    }
    if show_live {
        consumed += cell_width(&live_chip);
    }
    let message = choose_message(&tiers, width.saturating_sub(consumed));

    // Assemble in visual order.
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(
            project_chip,
            Style::default().fg(theme.accent_fg).bg(theme.accent),
        ),
        Span::raw(GAP),
    ];
    if show_focus {
        spans.push(Span::styled(
            focus_chip,
            Style::default().fg(theme.focus_indicator),
        ));
    }
    if show_live {
        spans.push(Span::styled(live_chip, Style::default().fg(theme.hint)));
    }
    if v.passthrough {
        // Reverse-video the passthrough chip so it stands out — a reminder that
        // wrk's normal Alt+… shortcuts won't fire while focus is on the shell
        // pane. Reuses theme slots so users who recolor `error` get a matching
        // chip background.
        spans.push(Span::styled(
            "[passthru] ",
            Style::default()
                .fg(theme.accent_fg)
                .bg(theme.error)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if v.select {
        spans.push(Span::styled(
            "[select] ",
            Style::default()
                .fg(theme.accent_fg)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if !message.is_empty() {
        spans.push(Span::styled(message, msg_style));
    }
    spans
}

/// Pick the widest message tier that fits in `budget`; if even the shortest
/// overflows, truncate it with an ellipsis.
fn choose_message(tiers: &[String], budget: usize) -> String {
    for t in tiers {
        if cell_width(t) <= budget {
            return t.clone();
        }
    }
    match tiers.last() {
        Some(t) => truncate_to_width(t, budget),
        None => String::new(),
    }
}

/// Display width of `s` in terminal cells (accounts for wide/zero-width chars).
fn cell_width(s: &str) -> usize {
    Span::raw(s).width()
}

/// Truncate `s` to at most `max` cells, appending `…` when it had to cut.
fn truncate_to_width(s: &str, max: usize) -> String {
    if cell_width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let budget = max - 1; // leave a cell for the ellipsis
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let mut buf = [0u8; 4];
        let cw = cell_width(ch.encode_utf8(&mut buf));
        if used + cw > budget {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

/// Focus-specific hint tiers from the live keymap (longest first), so the
/// status line can shed detail as the window narrows while staying in sync with
/// any user key overrides. Hard-coded projects-pane keys (`+`, `d`, `u`, `r`,
/// `/`, `↑`/`↓`, `Enter`) aren't configurable yet, so they're spliced literally.
fn hint_tiers(app: &App) -> Vec<String> {
    let km = &app.keymap;
    use crate::keymap::GlobalAction as A;
    if app.select_mode {
        return vec![
            format!(
                "select: drag to highlight, release to copy, Esc/{toggle} cancel",
                toggle = km.display(A::EnterSelectMode),
            ),
            "select: Esc cancel".to_string(),
        ];
    }
    if app.shell_passthrough && app.focus == Focus::Shell {
        let toggle = km.display(A::ToggleShellPassthrough);
        return vec![
            format!("{toggle} exit passthrough  (all other keys → shell)"),
            format!("{toggle} exit passthrough"),
        ];
    }
    let quit = km.display(A::Quit);
    let layout = km.display(A::ToggleLayout);
    match app.focus {
        Focus::Projects => {
            let sidebar = km.display(A::ToggleSidebar);
            let shrink = km.display(A::ShrinkClaude);
            let grow = km.display(A::GrowClaude);
            vec![
                format!(
                    "↑/↓ Enter/dbl-click  +/d/u/r  /  {sidebar} sidebar  \
                     {layout} layout  {shrink}/{grow} resize  {quit} quit"
                ),
                format!("↑/↓ Enter  +/d/u/r  {sidebar} sidebar  {layout} layout  {quit} quit"),
                format!("↑/↓ Enter  {quit} quit"),
                format!("{quit} quit"),
            ]
        }
        _ => {
            let fp = km.display(A::FocusProjects);
            let fc = km.display(A::FocusClaude);
            let fs = km.display(A::FocusShell);
            let new = km.display(A::NewClaudeTab);
            let md = km.display(A::OpenMarkdown);
            let close = km.display(A::CloseClaudeTab);
            let prev = km.display(A::PrevClaudeTab);
            let next = km.display(A::NextClaudeTab);
            let passthru = km.display(A::ToggleShellPassthrough);
            vec![
                format!(
                    "{fp}/{fc}/{fs} panes  {new} new-claude  {md} markdown  {close} close  \
                     {prev}/{next} tabs  {layout} layout  {passthru} passthru  {quit} quit"
                ),
                format!(
                    "{fp}/{fc}/{fs} panes  {new} new  {prev}/{next} tabs  {layout} layout  {quit} quit"
                ),
                format!("{fp}/{fc}/{fs} panes  {quit} quit"),
                format!("{quit} quit"),
            ]
        }
    }
}

#[derive(Debug, Clone)]
pub enum ModalState {
    Add(modal::AddProjectModal),
    OpenMarkdown(modal::OpenMarkdownModal),
    ConfirmDelete(modal::ConfirmDeleteModal),
    ConfirmUnload(modal::ConfirmUnloadModal),
    ConfirmQuit(modal::ConfirmQuitModal),
    ClaudeTabPicker(modal::ClaudeTabPickerModal),
    UrlPicker(modal::UrlPickerModal),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Theme;

    fn hint(tiers: &[&str]) -> StatusMsg {
        StatusMsg::Hint(tiers.iter().map(|s| s.to_string()).collect())
    }

    fn text_of(spans: &[Span<'static>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn wide_status_shows_all_context_and_the_full_hint() {
        let theme = Theme::default();
        let v = StatusView {
            project: "proj",
            focus_label: "claude",
            session_count: 3,
            layout_label: "split",
            passthrough: false,
            select: false,
            msg: hint(&["FULL panes tabs layout quit", "MED tabs quit", "quit"]),
        };
        let spans = compose_status(&theme, &v, 200);
        assert!(Line::from(spans.clone()).width() <= 200);
        let text = text_of(&spans);
        assert!(text.contains(" proj "));
        assert!(text.contains("[claude]"));
        assert!(text.contains("(3 live · split)"));
        assert!(text.contains("FULL panes tabs layout quit"));
    }

    #[test]
    fn narrow_status_drops_context_first_but_keeps_project_and_a_hint() {
        let theme = Theme::default();
        let v = StatusView {
            project: "proj",
            focus_label: "claude",
            session_count: 3,
            layout_label: "split",
            passthrough: false,
            select: false,
            msg: hint(&[
                "FULL hint that is definitely too wide",
                "panes quit",
                "quit",
            ]),
        };
        let width = 24;
        let spans = compose_status(&theme, &v, width);
        assert!(Line::from(spans.clone()).width() <= width);
        let text = text_of(&spans);
        assert!(text.contains("proj"), "project chip must survive");
        assert!(!text.contains("live"), "context chip should drop first");
        // A shorter hint tier still shows rather than nothing.
        assert!(text.contains("quit"));
    }

    #[test]
    fn warning_chips_survive_when_the_hint_collapses() {
        let theme = Theme::default();
        let v = StatusView {
            project: "proj",
            focus_label: "shell",
            session_count: 2,
            layout_label: "tabs",
            passthrough: true,
            select: true,
            msg: hint(&["a long hint that will not fit here", "quit"]),
        };
        let width = 40;
        let spans = compose_status(&theme, &v, width);
        assert!(Line::from(spans.clone()).width() <= width);
        let text = text_of(&spans);
        assert!(text.contains("[passthru]"));
        assert!(text.contains("[select]"));
    }

    #[test]
    fn very_narrow_truncates_an_error_with_an_ellipsis() {
        let theme = Theme::default();
        let v = StatusView {
            project: "p",
            focus_label: "claude",
            session_count: 1,
            layout_label: "split",
            passthrough: false,
            select: false,
            msg: StatusMsg::Error("error: something went badly wrong here".into()),
        };
        let width = 20;
        let spans = compose_status(&theme, &v, width);
        assert!(Line::from(spans.clone()).width() <= width);
        assert!(text_of(&spans).contains('…'));
    }

    #[test]
    fn truncate_to_width_respects_the_cell_budget() {
        assert_eq!(truncate_to_width("hello", 10), "hello");
        let t = truncate_to_width("hello world", 5);
        assert!(cell_width(&t) <= 5);
        assert!(t.ends_with('…'));
        assert_eq!(truncate_to_width("x", 0), "");
    }
}

#[cfg(test)]
mod render_check {
    use super::*;
    use crate::settings::Theme;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::widgets::{Paragraph, Widget};

    /// Drive the real `Paragraph` widget path across a range of widths. Once the
    /// window is wide enough to hold the mandatory project chip the composed line
    /// fits exactly (nothing silently clipped); below that the chip alone can't
    /// fit, so we only require that rendering never panics.
    #[test]
    fn renders_within_bounds_at_every_width() {
        let theme = Theme::default();
        let tiers = vec![
            "Alt+1/2/3 panes  Alt+n new-claude  Alt+m markdown  Alt+w close  \
             Alt+</Alt+> tabs  Alt+t layout  F12 passthru  Alt+q quit"
                .to_string(),
            "Alt+1/2/3 panes  Alt+n new  Alt+</Alt+> tabs  Alt+t layout  Alt+q quit".to_string(),
            "Alt+1/2/3 panes  Alt+q quit".to_string(),
            "Alt+q quit".to_string(),
        ];
        for width in [200u16, 120, 80, 60, 40, 30, 20, 12, 6, 1] {
            let v = StatusView {
                project: "wrk",
                focus_label: "claude",
                session_count: 3,
                layout_label: "split",
                passthrough: false,
                select: false,
                msg: StatusMsg::Hint(tiers.clone()),
            };
            let line = Line::from(compose_status(&theme, &v, width as usize));
            // " wrk " + separator = 6 cells; at or above that the line must fit.
            if width >= 6 {
                assert!(
                    line.width() <= width as usize,
                    "w={width} line={}",
                    line.width()
                );
            }
            let area = Rect::new(0, 0, width, 1);
            let mut buf = Buffer::empty(area);
            Paragraph::new(line).render(area, &mut buf);
        }
    }
}
