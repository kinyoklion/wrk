use nucleo_matcher::{
    Config, Matcher,
    pattern::{CaseMatching, Normalization, Pattern},
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, StatefulWidget};

use crate::store::ProjectStore;

pub struct ProjectSidebar {
    pub state: ListState,
    pub filter: Option<String>,
    pub focused: bool,
    /// Map from filtered display index → original store index.
    pub filtered_indices: Vec<usize>,
    /// Name of the currently active project (rendered with a marker).
    pub active: Option<String>,
}

impl Default for ProjectSidebar {
    fn default() -> Self {
        Self {
            state: ListState::default(),
            filter: None,
            focused: true,
            filtered_indices: Vec::new(),
            active: None,
        }
    }
}

impl ProjectSidebar {
    pub fn refresh(&mut self, store: &ProjectStore) {
        let names: Vec<&str> = store.projects.iter().map(|p| p.name.as_str()).collect();
        let filter_text = self.filter.as_deref().filter(|s| !s.is_empty());
        self.filtered_indices = match filter_text {
            None => (0..names.len()).collect(),
            Some(pat) => {
                let mut matcher = Matcher::new(Config::DEFAULT);
                let parsed = Pattern::parse(pat, CaseMatching::Ignore, Normalization::Smart);
                let mut buf = Vec::new();
                let mut scored: Vec<(usize, u32)> = names
                    .iter()
                    .enumerate()
                    .filter_map(|(i, n)| {
                        let haystack = nucleo_matcher::Utf32Str::new(n, &mut buf);
                        parsed.score(haystack, &mut matcher).map(|s| (i, s))
                    })
                    .collect();
                scored.sort_by(|a, b| b.1.cmp(&a.1));
                scored.into_iter().map(|(i, _)| i).collect()
            }
        };
        if self.filtered_indices.is_empty() {
            self.state.select(None);
        } else if self
            .state
            .selected()
            .is_none_or(|i| i >= self.filtered_indices.len())
        {
            self.state.select(Some(0));
        }
    }

    pub fn selected_store_index(&self) -> Option<usize> {
        self.state
            .selected()
            .and_then(|i| self.filtered_indices.get(i).copied())
    }

    pub fn select_next(&mut self) {
        if self.filtered_indices.is_empty() {
            return;
        }
        let i = self.state.selected().map_or(0, |i| {
            (i + 1).min(self.filtered_indices.len() - 1)
        });
        self.state.select(Some(i));
    }

    pub fn select_prev(&mut self) {
        if self.filtered_indices.is_empty() {
            return;
        }
        let i = self.state.selected().map_or(0, |i| i.saturating_sub(1));
        self.state.select(Some(i));
    }

    pub fn render(&mut self, area: Rect, buf: &mut Buffer, store: &ProjectStore) {
        let title = match &self.filter {
            Some(f) => format!(" projects /{f} "),
            None => " projects ".to_string(),
        };
        let border_style = if self.focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(border_style);

        let items: Vec<ListItem> = self
            .filtered_indices
            .iter()
            .filter_map(|&i| store.projects.get(i))
            .map(|p| {
                let marker = match &self.active {
                    Some(name) if name == &p.name => " *",
                    _ => "",
                };
                ListItem::new(Line::from(format!("{}{}", p.name, marker)))
            })
            .collect();

        let list = List::new(items).block(block).highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );

        StatefulWidget::render(list, area, buf, &mut self.state);
    }
}
