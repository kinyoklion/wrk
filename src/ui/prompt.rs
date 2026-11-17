//! The prompt a pane's process asks through `wrk confirm` (an IPC
//! [`ConfirmRequest`]): a question, a few lines of detail, and a yes/no answer
//! written back on the requester's connection.
//!
//! A prompt can appear while the user is typing into a pane, so keys pressed
//! in its first moments are ignored rather than taken as the answer.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Widget, Wrap};

use crate::ipc::{ConfirmRequest, Reply};
use crate::settings::Theme;
use crate::ui::bordered;

/// How long a new prompt ignores keys, so a keystroke meant for the pane
/// isn't read as the answer.
pub const ARM_AFTER: Duration = Duration::from_millis(600);

#[derive(Debug)]
pub struct PromptModal {
    /// Who is asking: the requesting project's display name.
    pub source: Option<String>,
    pub title: String,
    pub lines: Vec<String>,
    pub accept: String,
    pub decline: String,
    pub reply: Reply,
    /// When it was shown (`None` while queued behind another).
    pub shown_at: Option<Instant>,
}

impl PromptModal {
    pub fn new(req: ConfirmRequest, source: Option<String>, reply: Reply) -> Self {
        let label = |s: Option<String>, default: &str| {
            s.map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        PromptModal {
            source,
            title: req.title,
            lines: req.lines,
            accept: label(req.accept, "yes"),
            decline: label(req.decline, "no"),
            reply,
            shown_at: None,
        }
    }

    /// Mark it on screen from `now`.
    pub fn show(&mut self, now: Instant) {
        self.shown_at.get_or_insert(now);
    }

    /// Will a key answer it yet?
    pub fn armed(&self, now: Instant) -> bool {
        self.shown_at
            .is_some_and(|at| now.saturating_duration_since(at) >= ARM_AFTER)
    }

    /// Answer, closing the requester's connection.
    pub fn answer(&mut self, accepted: bool) {
        self.reply.answer(accepted);
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        let armed = self.armed(Instant::now());
        let title = match &self.source {
            Some(source) => format!(" {source} asks "),
            None => " confirm ".to_string(),
        };
        let mut text = vec![
            Line::from(Span::styled(
                self.title.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::default(),
        ];
        text.extend(self.lines.iter().map(|l| Line::from(l.clone())));
        // Lines plus the question, a gap, and the key hint; wrapped text may
        // need more, which the cap below trims.
        let longest = std::iter::once(&self.title)
            .chain(self.lines.iter())
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0) as u16;
        let width = (longest + 4)
            .max(50)
            .min(area.width.saturating_sub(4))
            .max(10);
        let inner_width = width.saturating_sub(2).max(1) as usize;
        let body_rows: usize = text
            .iter()
            .map(|l| l.width().max(1).div_ceil(inner_width))
            .sum();
        let height = (body_rows as u16 + 4)
            .min(area.height.saturating_sub(2))
            .max(5);
        let popup = Rect {
            x: area.x + area.width.saturating_sub(width) / 2,
            y: area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        };
        Clear.render(popup, buf);
        let block = bordered(theme)
            .title(title)
            .border_style(Style::default().fg(theme.accent));
        let inner = block.inner(popup);
        block.render(popup, buf);
        let hint_row = Rect {
            y: inner.y + inner.height.saturating_sub(1),
            height: 1.min(inner.height),
            ..inner
        };
        let body = Rect {
            height: inner.height.saturating_sub(2),
            ..inner
        };
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .render(body, buf);
        let hint = format!("y: {}   n / Esc: {}", self.accept, self.decline);
        let style = if armed {
            Style::default().fg(theme.hint)
        } else {
            Style::default().fg(theme.hint).add_modifier(Modifier::DIM)
        };
        Paragraph::new(hint).style(style).render(hint_row, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ConfirmRequest {
        ConfirmRequest {
            project: Some("boxes/i-1".into()),
            title: "dev-box wants to send 2 files (1.2 MB)".into(),
            lines: vec!["report.md  12 KB".into(), "img/a.png  1.2 MB".into()],
            accept: Some("download".into()),
            decline: None,
        }
    }

    fn rendered(m: &PromptModal) -> String {
        let area = Rect::new(0, 0, 80, 20);
        let mut buf = Buffer::empty(area);
        m.render(area, &mut buf, &Theme::default());
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn it_names_the_asker_the_question_and_the_answers() {
        let mut m = PromptModal::new(request(), Some("dev-box".into()), Reply::default());
        m.show(Instant::now());
        let screen = rendered(&m);
        assert!(screen.contains("dev-box asks"), "{screen}");
        assert!(screen.contains("wants to send 2 files"), "{screen}");
        assert!(screen.contains("img/a.png"), "{screen}");
        assert!(screen.contains("y: download   n / Esc: no"), "{screen}");
    }

    #[test]
    fn keys_count_only_once_it_has_been_up_a_moment() {
        let mut m = PromptModal::new(request(), None, Reply::default());
        let now = Instant::now();
        assert!(!m.armed(now), "queued, not shown");
        m.show(now);
        assert!(!m.armed(now));
        assert!(m.armed(now + ARM_AFTER));
        // Showing again doesn't restart the clock.
        m.show(now + ARM_AFTER);
        assert!(m.armed(now + ARM_AFTER));
    }
}
